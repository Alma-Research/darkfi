use drk::blockchain::{rocks::columns, Rocks, RocksColumn, Slab};
use drk::cli::TransferParams;
use drk::cli::{Config, DarkfidCli, DarkfidConfig};
use drk::crypto::{
    load_params,
    merkle::{CommitmentTree, IncrementalWitness},
    merkle_node::MerkleNode,
    note::{EncryptedNote, Note},
    nullifier::Nullifier,
    save_params, setup_mint_prover, setup_spend_prover,
};
use drk::rpc::adapters::user_adapter::UserAdapter;
use drk::rpc::jsonserver;
use drk::serial::{deserialize, Decodable};
use drk::service::{CashierClient, GatewayClient, GatewaySlabsSubscriber};
use drk::state::{state_transition, ProgramState, StateUpdate};
use drk::util::{join_config_path, prepare_transaction};
use drk::wallet::{WalletDb, WalletPtr};
use drk::{tx, Result};

use async_executor::Executor;
use bellman::groth16;
use bls12_381::Bls12;
use easy_parallel::Parallel;
use ff::Field;
use log::*;
use rand::rngs::OsRng;
use rusqlite::Connection;

use async_std::sync::Arc;
use futures::FutureExt;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;

pub struct State {
    // The entire merkle tree state
    tree: CommitmentTree<MerkleNode>,
    // List of all previous and the current merkle roots
    // This is the hashed value of all the children.
    merkle_roots: RocksColumn<columns::MerkleRoots>,
    // Nullifiers prevent double spending
    nullifiers: RocksColumn<columns::Nullifiers>,
    // Mint verifying key used by ZK
    mint_pvk: groth16::PreparedVerifyingKey<Bls12>,
    // Spend verifying key used by ZK
    spend_pvk: groth16::PreparedVerifyingKey<Bls12>,
    // Public key of the cashier
    // List of all our secret keys
    wallet: WalletPtr,
}

impl ProgramState for State {
    fn is_valid_cashier_public_key(&self, _public: &jubjub::SubgroupPoint) -> bool {
        let conn = Connection::open(&self.wallet.path).expect("Failed to connect to database");
        let mut stmt = conn
            .prepare("SELECT key_public FROM cashier WHERE key_public IN (SELECT key_public)")
            .expect("Cannot generate statement.");
        stmt.exists([1i32]).expect("Failed to read database")
        // do actual validity check
    }

    fn is_valid_merkle(&self, merkle_root: &MerkleNode) -> bool {
        self.merkle_roots
            .key_exist(*merkle_root)
            .expect("couldn't check if the merkle_root valid")
    }

    fn nullifier_exists(&self, nullifier: &Nullifier) -> bool {
        self.nullifiers
            .key_exist(nullifier.repr)
            .expect("couldn't check if nullifier exists")
    }

    // load from disk
    fn mint_pvk(&self) -> &groth16::PreparedVerifyingKey<Bls12> {
        &self.mint_pvk
    }

    fn spend_pvk(&self) -> &groth16::PreparedVerifyingKey<Bls12> {
        &self.spend_pvk
    }
}

impl State {
    async fn apply(&mut self, update: StateUpdate) -> Result<()> {
        // Extend our list of nullifiers with the ones from the update
        for nullifier in update.nullifiers {
            self.nullifiers.put(nullifier, vec![] as Vec<u8>)?;
        }

        // Update merkle tree and witnesses
        for (coin, enc_note) in update.coins.into_iter().zip(update.enc_notes.into_iter()) {
            // Add the new coins to the merkle tree
            let node = MerkleNode::from_coin(&coin);
            self.tree.append(node).expect("Append to merkle tree");

            // Keep track of all merkle roots that have existed
            self.merkle_roots.put(self.tree.root(), vec![] as Vec<u8>)?;

            // Also update all the coin witnesses
            for witness in self.wallet.witnesses.lock().await.iter_mut() {
                witness.append(node).expect("append to witness");
            }

            if let Some((note, secret)) = self.try_decrypt_note(enc_note).await {
                // We need to keep track of the witness for this coin.
                // This allows us to prove inclusion of the coin in the merkle tree with ZK.
                // Just as we update the merkle tree with every new coin, so we do the same with
                // the witness.

                // Derive the current witness from the current tree.
                // This is done right after we add our coin to the tree (but before any other
                // coins are added)

                // Make a new witness for this coin
                let witness = IncrementalWitness::from_tree(&self.tree);

                self.wallet
                    .put_own_coins(coin.clone(), note.clone(), witness.clone(), secret)?;
            }
        }
        Ok(())
    }

    async fn try_decrypt_note(&self, ciphertext: EncryptedNote) -> Option<(Note, jubjub::Fr)> {
        let secret = self.wallet.get_private().ok()?;
        match ciphertext.decrypt(&secret) {
            Ok(note) => {
                // ... and return the decrypted note for this coin.
                return Some((note, secret.clone()));
            }
            Err(_) => {}
        }
        // We weren't able to decrypt the note with our key.
        None
    }
}

//pub async fn subscribe(
//    gateway_slabs_sub: GatewaySlabsSubscriber,
//    mut state: State,
//) -> Result<()> {
//}

pub async fn futures_broker(
    client: &mut GatewayClient,
    cashier_client: &mut CashierClient,
    state: &mut State,
    secret: jubjub::Fr,
    mint_params: bellman::groth16::Parameters<Bls12>,
    spend_params: bellman::groth16::Parameters<Bls12>,
    gateway_slabs_sub: async_channel::Receiver<Slab>,
    deposit_req: async_channel::Receiver<jubjub::SubgroupPoint>,
    deposit_rep: async_channel::Sender<Option<bitcoin::util::address::Address>>,
    withdraw_req: async_channel::Receiver<String>,
    withdraw_rep: async_channel::Sender<Option<jubjub::SubgroupPoint>>,
    publish_tx_recv: async_channel::Receiver<TransferParams>,
) -> Result<()> {
    loop {
        futures::select! {
            slab = gateway_slabs_sub.recv().fuse() => {
                let slab = slab?;
                let tx = tx::Transaction::decode(&slab.get_payload()[..])?;
                let update = state_transition(state, tx)?;
                state.apply(update).await?;
            }
            deposit_addr = deposit_req.recv().fuse() => {
                let btc_public = cashier_client.get_address(deposit_addr?).await?;
                deposit_rep.send(btc_public).await?;
            }
            withdraw_addr = withdraw_req.recv().fuse() => {
                let drk_public = cashier_client.withdraw(withdraw_addr?).await?;
                withdraw_rep.send(drk_public).await?;
            }
            transfer_params = publish_tx_recv.recv().fuse() => {
                let transfer_params = transfer_params?;

                let address = bs58::decode(transfer_params.pub_key).into_vec()?;
                let address: jubjub::SubgroupPoint = deserialize(&address)?;

                let own_coins = state.wallet.get_own_coins()?;

                let slab = prepare_transaction(
                    state,
                    secret.clone(),
                    mint_params.clone(),
                    spend_params.clone(),
                    address,
                    transfer_params.amount,
                    own_coins
                )?;

                client.put_slab(slab).await.expect("put slab");
            }

        }
    }
}

async fn start(executor: Arc<Executor<'_>>, config: Arc<DarkfidConfig>) -> Result<()> {
    let connect_addr: SocketAddr = config.connect_url.parse()?;
    let sub_addr: SocketAddr = config.subscriber_url.parse()?;
    let cashier_addr: SocketAddr = config.cashier_url.parse()?;
    let database_path = config.database_path.clone();
    let walletdb_path = config.walletdb_path.clone();

    let database_path = join_config_path(&PathBuf::from(database_path))?;
    let walletdb_path = join_config_path(&PathBuf::from(walletdb_path))?;

    let rocks = Rocks::new(&database_path)?;

    let rocks2 = rocks.clone();
    let slabstore = RocksColumn::<columns::Slabs>::new(rocks2.clone());

    // Auto create trusted ceremony parameters if they don't exist
    if !Path::new("mint.params").exists() {
        let params = setup_mint_prover();
        save_params("mint.params", &params)?;
    }
    if !Path::new("spend.params").exists() {
        let params = setup_spend_prover();
        save_params("spend.params", &params)?;
    }

    // Load trusted setup parameters
    let (mint_params, mint_pvk) = load_params("mint.params")?;
    let (spend_params, spend_pvk) = load_params("spend.params")?;

    //let cashier_secret = jubjub::Fr::random(&mut OsRng);
    //let cashier_public = zcash_primitives::constants::SPENDING_KEY_GENERATOR * cashier_secret;

    // wallet secret key
    let secret = jubjub::Fr::random(&mut OsRng);
    // wallet public key
    let _public = zcash_primitives::constants::SPENDING_KEY_GENERATOR * secret;

    let merkle_roots = RocksColumn::<columns::MerkleRoots>::new(rocks.clone());
    let nullifiers = RocksColumn::<columns::Nullifiers>::new(rocks);

    let wallet = Arc::new(WalletDb::new(&walletdb_path, config.password.clone())?);

    let ex = executor.clone();

    let mut state = State {
        tree: CommitmentTree::empty(),
        merkle_roots,
        nullifiers,
        mint_pvk,
        spend_pvk,
        wallet: wallet.clone(),
    };

    // create gateway client
    debug!(target: "Client", "Creating client");
    let mut client = GatewayClient::new(connect_addr, sub_addr, slabstore)?;

    // create cashier client
    debug!(target: "Cashier Client", "Creating cashier client");
    let mut cashier_client = CashierClient::new(cashier_addr)?;

    debug!(target: "Gateway", "Start subscriber");
    // start subscribing
    let gateway_slabs_sub: GatewaySlabsSubscriber =
        client.start_subscriber(executor.clone()).await?;

    // channels to request transfer from adapter
    let (publish_tx_send, publish_tx_recv) = async_channel::unbounded::<TransferParams>();

    // channels to request deposit from adapter, send DRK key and receive BTC key
    let (deposit_req_send, deposit_req_recv) = async_channel::unbounded::<jubjub::SubgroupPoint>();
    let (deposit_rep_send, deposit_rep_recv) =
        async_channel::unbounded::<Option<bitcoin::util::address::Address>>();

    // channel to request withdraw from adapter, send BTC key and receive DRK key
    let (withdraw_req_send, withdraw_req_recv) = async_channel::unbounded::<String>();
    let (withdraw_rep_send, withdraw_rep_recv) =
        async_channel::unbounded::<Option<jubjub::SubgroupPoint>>();

    // start gateway client
    debug!(target: "fn::start client", "start() Client started");
    client.start().await?;
    cashier_client.start().await?;

    let futures_broker_task = executor.spawn(async move {
        futures_broker(
            &mut client,
            &mut cashier_client,
            &mut state,
            secret.clone(),
            mint_params.clone(),
            spend_params.clone(),
            gateway_slabs_sub.clone(),
            deposit_req_recv.clone(),
            deposit_rep_send.clone(),
            withdraw_req_recv.clone(),
            withdraw_rep_send.clone(),
            publish_tx_recv.clone(),
        )
        .await?;
        Ok::<(), drk::Error>(())
    });

    let adapter = Arc::new(UserAdapter::new(
        wallet.clone(),
        publish_tx_send,
        (deposit_req_send, deposit_rep_recv),
        (withdraw_req_send, withdraw_rep_recv),
    )?);

    let rpc_url: std::net::SocketAddr = config.rpc_url.parse()?;

    // start the rpc server
    let io = Arc::new(adapter.handle_input()?);
    jsonserver::start(ex.clone(), rpc_url, io).await?;

    futures_broker_task.cancel().await;
    Ok(())
}

fn main() -> Result<()> {
    let options = Arc::new(DarkfidCli::load()?);

    let config_path: PathBuf;

    match options.config.as_ref() {
        Some(path) => {
            config_path = path.to_owned();
        }
        None => {
            config_path = join_config_path(&PathBuf::from("darkfid.toml"))?;
        }
    }

    let config: DarkfidConfig = if Path::new(&config_path).exists() {
        Config::<DarkfidConfig>::load(config_path)?
    } else {
        Config::<DarkfidConfig>::load_default(config_path)?
    };

    let config = Arc::new(config);

    let ex = Arc::new(Executor::new());
    let (signal, shutdown) = async_channel::unbounded::<()>();

    {
        use simplelog::*;
        let logger_config = ConfigBuilder::new().set_time_format_str("%T%.6f").build();

        let debug_level = if options.verbose {
            LevelFilter::Debug
        } else {
            LevelFilter::Off
        };

        let log_path = config.log_path.clone();
        CombinedLogger::init(vec![
            TermLogger::new(debug_level, logger_config, TerminalMode::Mixed).unwrap(),
            WriteLogger::new(
                LevelFilter::Debug,
                Config::default(),
                std::fs::File::create(log_path).unwrap(),
            ),
        ])
        .unwrap();
    }

    let ex2 = ex.clone();

    let (_, result) = Parallel::new()
        // Run four executor threads.
        .each(0..3, |_| smol::future::block_on(ex.run(shutdown.recv())))
        // Run the main future on the current thread.
        .finish(|| {
            smol::future::block_on(async move {
                start(ex2, config).await?;
                drop(signal);
                Ok::<(), drk::Error>(())
            })
        });

    result
}