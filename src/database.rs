use std::{net, time};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use dht::{self, DHT};
use version_vector::*;
use fabric::*;
use vnode::*;
use workers::*;
use resp::RespValue;
use storage::{StorageManager, Storage};

pub type NodeId = u64;
pub type Token = u64;
pub type Cookie = (u64, u64);
pub type VNodeId = u16;

pub type DatabaseResponseFn = Box<Fn(Token, RespValue) + Send + Sync>;

pub struct Database {
    pub dht: DHT<()>,
    pub fabric: Fabric,
    pub meta_storage: Storage,
    pub storage_manager: StorageManager,
    workers: Mutex<WorkerManager>,
    vnodes: RwLock<HashMap<VNodeId, Mutex<VNode>>>,
    inflight: Mutex<HashMap<Cookie, ProxyReqState>>,
    pub response_fn: DatabaseResponseFn,
}

struct ProxyReqState {
    from: NodeId,
    cookie: Cookie,
}

macro_rules! vnode {
    ($s: expr, $k: expr, $ok: expr) => ({
        let vnodes = $s.vnodes.read().unwrap();
        vnodes.get(&$k).map(|vn| vn.lock().unwrap()).map($ok);
    });
    ($s: expr, $k: expr, $status: expr, $ok: expr) => ({
        let vnodes = $s.vnodes.read().unwrap();
        match vnodes.get(&$k).map(|vn| vn.lock().unwrap()).and_then(|vn| {
            match vn.status() {
                status => Some(vn),
                _ => None
            }
        }).map($ok);
    });
}

impl Database {
    pub fn new(node: NodeId, fabric_addr: net::SocketAddr, storage_dir: &str, is_create: bool,
               response_fn: DatabaseResponseFn)
               -> Arc<Database> {
        let storage_manager = StorageManager::new(storage_dir).unwrap();
        let meta_storage = storage_manager.open(-1, true).unwrap();
        let workers = WorkerManager::new(1, time::Duration::from_millis(1000));
        let db = Arc::new(Database {
            fabric: Fabric::new(node, fabric_addr).unwrap(),
            dht: DHT::new(node,
                          fabric_addr,
                          "test",
                          if is_create {
                              Some(((), dht::RingDescription::new(3, 64)))
                          } else {
                              None
                          }),
            storage_manager: storage_manager,
            meta_storage: meta_storage,
            inflight: Default::default(),
            response_fn: response_fn,
            vnodes: Default::default(),
            workers: Mutex::new(workers),
        });

        db.workers.lock().unwrap().start(|| {
            let cdb = Arc::downgrade(&db);
            Box::new(move |chan| {
                for wm in chan {
                    trace!("worker got msg {:?}", wm);
                    let db = if let Some(db) = cdb.upgrade() {
                        db
                    } else {
                        break;
                    };
                    match wm {
                        WorkerMsg::Fabric(from, m) => db.handler_fabric_msg(from, m),
                        WorkerMsg::Tick(time) => db.handler_tick(time),
                        WorkerMsg::Command(token, cmd) => db.handler_cmd(token, cmd),
                        WorkerMsg::DHTChange => db.handler_dht_change(),
                        WorkerMsg::Exit => break,
                    }
                }
                info!("Exiting worker")
            })
        });

        // FIXME: DHT callback shouldnt require sync
        let sender = Mutex::new(db.sender());
        db.dht.set_callback(Box::new(move || {
            sender.lock().unwrap().send(WorkerMsg::DHTChange);
        }));

        // register nodes into fabric
        db.dht.members().into_iter().map(|(n, a)| db.fabric.register_node(n, a)).count();
        // FIXME: fabric should have a start method that receives the callbacks
        // set fabric callbacks
        for &msg_type in &[FabricMsgType::Crud, FabricMsgType::Synch, FabricMsgType::Bootstrap] {
            let mut sender = db.sender();
            db.fabric.register_msg_handler(msg_type,
                                           Box::new(move |f, m| {
                                               sender.send(WorkerMsg::Fabric(f, m));
                                           }));
        }

        {
            // acquire exclusive lock to vnodes to intialize them
            let mut vnodes = db.vnodes.write().unwrap();
            let (ready_vnodes, pending_vnodes) = db.dht.vnodes_for_node(db.dht.node());
            // create vnodes
            *vnodes = (0..db.dht.partitions() as VNodeId)
                .map(|i| {
                    let vn = if ready_vnodes.contains(&i) {
                        VNode::new(&db, i, VNodeStatus::Ready, is_create)
                    } else if pending_vnodes.contains(&i) {
                        VNode::new(&db, i, VNodeStatus::Bootstrap, is_create)
                    } else {
                        VNode::new(&db, i, VNodeStatus::Absent, is_create)
                    };
                    (i, Mutex::new(vn))
                })
                .collect();
        }

        db
    }

    pub fn save(&self, shutdown: bool) {
        for vn in self.vnodes.read().unwrap().values() {
            vn.lock().unwrap().save(self, shutdown);
        }
    }

    // FIXME: leaky abstraction
    pub fn sender(&self) -> WorkerSender {
        self.workers.lock().unwrap().sender()
    }

    fn handler_dht_change(&self) {
        for (node, meta) in self.dht.members() {
            self.fabric.register_node(node, meta);
        }

        for (&i, vn) in self.vnodes.read().unwrap().iter() {
            let final_status = if self.dht.nodes_for_vnode(i, true).contains(&self.dht.node()) {
                VNodeStatus::Ready
            } else {
                VNodeStatus::Absent
            };
            vn.lock().unwrap().handler_dht_change(self, final_status);
        }
    }

    fn handler_tick(&self, time: time::Instant) {
        for vn in self.vnodes.read().unwrap().values() {
            vn.lock().unwrap().handler_tick(self, time);
        }
    }

    fn handler_fabric_msg(&self, from: NodeId, msg: FabricMsg) {
        match msg {
            FabricMsg::RemoteGet(m) => self.handler_get_remote(from, m),
            FabricMsg::RemoteGetAck(m) => self.handler_get_remote_ack(from, m),
            FabricMsg::Set(m) => self.handler_set(from, m),
            FabricMsg::SetAck(m) => self.handler_set_ack(from, m),
            FabricMsg::RemoteSet(m) => self.handler_set_remote(from, m),
            FabricMsg::RemoteSetAck(m) => self.handler_set_remote_ack(from, m),
            FabricMsg::BootstrapStart(m) => {
                vnode!(self, m.vnode, |mut vn| vn.handler_bootstrap_start(self, from, m))
            }
            FabricMsg::BootstrapSend(m) => {
                vnode!(self, m.vnode, |mut vn| vn.handler_bootstrap_send(self, from, m))
            }
            FabricMsg::BootstrapAck(m) => {
                vnode!(self, m.vnode, |mut vn| vn.handler_bootstrap_ack(self, from, m))
            }
            FabricMsg::BootstrapFin(m) => {
                vnode!(self, m.vnode, |mut vn| vn.handler_bootstrap_fin(self, from, m))
            }
            FabricMsg::SyncStart(m) => {
                vnode!(self, m.vnode, |mut vn| vn.handler_sync_start(self, from, m))
            }
            FabricMsg::SyncSend(m) => {
                vnode!(self, m.vnode, |mut vn| vn.handler_sync_send(self, from, m))
            }
            FabricMsg::SyncAck(m) => {
                vnode!(self, m.vnode, |mut vn| vn.handler_sync_ack(self, from, m))
            }
            FabricMsg::SyncFin(m) => {
                vnode!(self, m.vnode, |mut vn| vn.handler_sync_fin(self, from, m))
            }
            msg @ _ => unreachable!("Can't handle {:?}", msg),
        };
    }

    fn migrations_inflight(&self) -> usize {
        self.vnodes
            .read()
            .unwrap()
            .values()
            .map(|vn| vn.lock().unwrap().migrations_inflight())
            .sum()
    }

    fn syncs_inflight(&self) -> usize {
        self.vnodes.read().unwrap().values().map(|vn| vn.lock().unwrap().syncs_inflight()).sum()
    }

    fn start_migration(&self, vnode: VNodeId) {
        let vnodes = self.vnodes.read().unwrap();
        vnodes.get(&vnode).unwrap().lock().unwrap().start_migration(self);
    }

    fn start_sync(&self, vnode: VNodeId, reverse: bool) {
        let vnodes = self.vnodes.read().unwrap();
        vnodes.get(&vnode).unwrap().lock().unwrap().start_sync(self, reverse);
    }

    fn send_set(&self, addr: NodeId, vnode: VNodeId, cookie: Cookie, key: &[u8],
                value_opt: Option<&[u8]>, vv: VersionVector) {
        self.fabric
            .send_message(addr,
                          FabricMsg::Set(MsgSet {
                              cookie: cookie,
                              vnode: vnode,
                              key: key.into(),
                              value: value_opt.map(|x| x.into()),
                              version_vector: vv,
                          }))
            .unwrap();
    }

    // CLIENT CRUD
    pub fn set(&self, token: Token, key: &[u8], value: Option<&[u8]>, vv: VersionVector) {
        let vnode = self.dht.key_vnode(key);
        vnode!(self, vnode, |mut vn| {
            vn.do_set(self, token, key, value, vv);
        });
    }

    pub fn get(&self, token: Token, key: &[u8]) {
        let vnode = self.dht.key_vnode(key);
        vnode!(self, vnode, |mut vn| {
            vn.do_get(self, token, key);
        });
    }

    // CRUD HANDLERS
    fn handler_set(&self, from: NodeId, msg: MsgSet) {
        vnode!(self, msg.vnode, |mut vn| {
            vn.handler_set(self, from, msg);
        });
    }

    fn handler_set_ack(&self, _from: NodeId, _msg: MsgSetAck) {}

    fn handler_set_remote(&self, from: NodeId, msg: MsgRemoteSet) {
        vnode!(self, msg.vnode, |mut vn| {
            vn.handler_set_remote(self, from, msg);
        });
    }

    fn handler_set_remote_ack(&self, from: NodeId, msg: MsgRemoteSetAck) {
        vnode!(self, msg.vnode, |mut vn| {
            vn.handler_set_remote_ack(self, from, msg);
        });
    }

    fn handler_get_remote(&self, from: NodeId, msg: MsgRemoteGet) {
        vnode!(self, msg.vnode, |mut vn| {
            vn.handler_get_remote(self, from, msg);
        });
    }

    fn handler_get_remote_ack(&self, from: NodeId, msg: MsgRemoteGetAck) {
        vnode!(self, msg.vnode, |mut vn| {
            vn.handler_get_remote_ack(self, from, msg);
        });
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // force dropping vnodes before other components
        let _ = self.vnodes.write().map(|mut vns| vns.clear());
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, net, fs, ops};
    use std::sync::{Mutex, Arc};
    use std::collections::HashMap;
    use super::*;
    use version_vector::{DottedCausalContainer, VersionVector};
    use env_logger;
    use bincode::{serde as bincode_serde, SizeLimit};
    use resp::RespValue;

    struct TestDatabase {
        db: Arc<Database>,
        responses: Arc<Mutex<HashMap<Token, RespValue>>>,
    }

    impl TestDatabase {
        fn new(node: NodeId, bind_addr: net::SocketAddr, storage_dir: &str, create: bool) -> Self {
            let responses1 = Arc::new(Mutex::new(HashMap::new()));
            let responses2 = responses1.clone();
            let db = Database::new(node,
                                   bind_addr,
                                   storage_dir,
                                   create,
                                   Box::new(move |t, v| {
                                       let r = responses1.lock().unwrap().insert(t, v);
                                       assert!(r.is_none(), "replaced a result");
                                   }));
            TestDatabase {
                db: db,
                responses: responses2,
            }
        }

        fn response(&self, token: Token) -> Option<DottedCausalContainer<Vec<u8>>> {
            (0..200)
                .filter_map(|_| {
                    thread::sleep_ms(10);
                    self.responses.lock().unwrap().remove(&token).and_then(|v| resp_to_dcc(v))
                })
                .next()
        }
    }

    impl ops::Deref for TestDatabase {
        type Target = Database;
        fn deref(&self) -> &Self::Target {
            &self.db
        }
    }

    fn resp_to_dcc(value: RespValue) -> Option<DottedCausalContainer<Vec<u8>>> {
        match value {
            RespValue::Data(bytes) => bincode_serde::deserialize(&bytes).ok(),
            _ => None,
        }
    }

    fn test_reload_stub(shutdown: bool) {
        let _ = fs::remove_dir_all("./t");
        let _ = env_logger::init();
        let mut db = TestDatabase::new(1, "127.0.0.1:9000".parse().unwrap(), "t/db", true);

        db.get(1, b"test");
        assert!(db.response(1).unwrap().is_empty());

        db.set(1, b"test", Some(b"value1"), VersionVector::new());
        assert!(db.response(1).unwrap().is_empty());

        db.get(1, b"test");
        assert!(db.response(1).unwrap().values().eq(vec![b"value1"]));

        db.save(shutdown);
        drop(db);
        db = TestDatabase::new(1, "127.0.0.1:9000".parse().unwrap(), "t/db", false);

        db.get(1, b"test");
        assert!(db.response(1).unwrap().values().eq(vec![b"value1"]));

        assert_eq!(1,
                   db.vnodes
                       .read()
                       .unwrap()
                       .values()
                       .map(|vn| vn.lock().unwrap()._log_len())
                       .sum::<usize>());
    }

    #[test]
    fn test_reload_shutdown() {
        test_reload_stub(true);
    }

    #[test]
    fn test_reload_dirty() {
        test_reload_stub(false);
    }

    #[test]
    fn test_one() {
        let _ = fs::remove_dir_all("./t");
        let _ = env_logger::init();
        let db = TestDatabase::new(1, "127.0.0.1:9000".parse().unwrap(), "t/db", true);
        db.get(1, b"test");
        assert!(db.response(1).unwrap().is_empty());

        db.set(1, b"test", Some(b"value1"), VersionVector::new());
        assert!(db.response(1).unwrap().is_empty());

        db.get(1, b"test");
        assert!(db.response(1).unwrap().values().eq(vec![b"value1"]));

        db.set(1, b"test", Some(b"value2"), VersionVector::new());
        assert!(db.response(1).unwrap().is_empty());

        db.get(1, b"test");
        let state = db.response(1).unwrap();
        assert!(state.values().eq(vec![b"value1", b"value2"]));

        db.set(1, b"test", Some(b"value12"), state.version_vector().clone());
        assert!(db.response(1).unwrap().is_empty());

        db.get(1, b"test");
        let state = db.response(1).unwrap();
        assert!(state.values().eq(vec![b"value12"]));

        db.set(1, b"test", None, state.version_vector().clone());
        assert!(db.response(1).unwrap().is_empty());

        db.get(1, b"test");
        assert!(db.response(1).unwrap().is_empty());
    }

    #[test]
    fn test_two() {
        let _ = fs::remove_dir_all("./t");
        let _ = env_logger::init();
        let db1 = TestDatabase::new(1, "127.0.0.1:9000".parse().unwrap(), "t/db1", true);
        let db2 = TestDatabase::new(2, "127.0.0.1:9001".parse().unwrap(), "t/db2", false);
        db2.dht.claim(db2.dht.node(), ());

        thread::sleep_ms(1000);
        while db1.migrations_inflight() + db2.migrations_inflight() > 0 {
            warn!("waiting for migrations to finish");
            thread::sleep_ms(1000);
        }

        db1.get(1, b"test");
        assert!(db1.response(1).unwrap().is_empty());

        db1.set(1, b"test", Some(b"value1"), VersionVector::new());
        assert!(db1.response(1).unwrap().is_empty());

        for &db in &[&db1, &db2] {
            db.get(1, b"test");
            assert!(db.response(1).unwrap().values().eq(vec![b"value1"]));
        }
    }

    const TEST_JOIN_SIZE: u64 = 10;

    #[test]
    fn test_join_migrate() {
        let _ = fs::remove_dir_all("./t");
        let _ = env_logger::init();
        let db1 = TestDatabase::new(1, "127.0.0.1:9000".parse().unwrap(), "t/db1", true);
        for i in 0..TEST_JOIN_SIZE {
            db1.set(i,
                    i.to_string().as_bytes(),
                    Some(i.to_string().as_bytes()),
                    VersionVector::new());
            db1.response(i).unwrap();
        }
        for i in 0..TEST_JOIN_SIZE {
            db1.get(i, i.to_string().as_bytes());
            assert!(db1.response(i).unwrap().values().eq(&[i.to_string().as_bytes()]));
        }

        let db2 = TestDatabase::new(2, "127.0.0.1:9001".parse().unwrap(), "t/db2", false);
        warn!("will check data in db2 before balancing");
        for i in 0..TEST_JOIN_SIZE {
            db2.get(i, i.to_string().as_bytes());
            assert!(db2.response(i).unwrap().values().eq(&[i.to_string().as_bytes()]));
        }

        db2.dht.claim(db2.dht.node(), ());

        // warn!("will check data in db2 during balancing");
        // for i in 0..TEST_JOIN_SIZE {
        //     db2.get(i, i.to_string().as_bytes());
        //     let result = db2.response(i);
        //     assert!(result.unwrap().values().eq(&[i.to_string().as_bytes()]));
        // }

        thread::sleep_ms(1000);
        while db1.migrations_inflight() + db2.migrations_inflight() > 0 {
            warn!("waiting for migrations to finish");
            thread::sleep_ms(1000);
        }

        // drop(db1);

        warn!("will check data in db2 after balancing");
        for i in 0..TEST_JOIN_SIZE {
            db2.get(i, i.to_string().as_bytes());
            assert!(db2.response(i).unwrap().values().eq(&[i.to_string().as_bytes()]));
        }
    }

    #[test]
    fn test_join_sync_reverse() {
        let _ = fs::remove_dir_all("./t");
        let _ = env_logger::init();
        let mut db1 = TestDatabase::new(1, "127.0.0.1:9000".parse().unwrap(), "t/db1", true);
        let mut db2 = TestDatabase::new(2, "127.0.0.1:9001".parse().unwrap(), "t/db2", false);
        db2.dht.claim(db2.dht.node(), ());

        thread::sleep_ms(1000);
        while db1.migrations_inflight() + db2.migrations_inflight() > 0 {
            warn!("waiting for migrations to finish");
            thread::sleep_ms(1000);
        }

        for i in 0..TEST_JOIN_SIZE {
            db1.set(i,
                    i.to_string().as_bytes(),
                    Some(i.to_string().as_bytes()),
                    VersionVector::new());
            db1.response(i).unwrap();
        }
        for i in 0..TEST_JOIN_SIZE {
            db1.get(i, i.to_string().as_bytes());
            let result1 = db1.response(i);
            db2.get(i, i.to_string().as_bytes());
            let result2 = db2.response(i);
            assert_eq!(result1, result2);
        }

        // sim unclean shutdown
        drop(db1);
        let _ = fs::remove_dir_all("t/db1");
        db1 = TestDatabase::new(1, "127.0.0.1:9000".parse().unwrap(), "t/db1", false);

        thread::sleep_ms(1000);
        while db1.syncs_inflight() + db2.syncs_inflight() > 0 {
            warn!("waiting for syncs to finish");
            thread::sleep_ms(1000);
        }

        warn!("will check data in db1 after sync");
        for i in 0..TEST_JOIN_SIZE {
            db1.get(i, i.to_string().as_bytes());
            assert!(db1.response(i).unwrap().values().eq(&[i.to_string().as_bytes()]));
        }
    }

    #[test]
    fn test_join_sync_normal() {
        let _ = fs::remove_dir_all("./t");
        let _ = env_logger::init();
        let mut db1 = TestDatabase::new(1, "127.0.0.1:9000".parse().unwrap(), "t/db1", true);
        let mut db2 = TestDatabase::new(2, "127.0.0.1:9001".parse().unwrap(), "t/db2", false);
        db2.dht.claim(db2.dht.node(), ());

        thread::sleep_ms(1000);
        while db1.migrations_inflight() + db2.migrations_inflight() > 0 {
            warn!("waiting for migrations to finish");
            thread::sleep_ms(1000);
        }

        for i in 0..TEST_JOIN_SIZE {
            db1.set(i,
                    i.to_string().as_bytes(),
                    Some(i.to_string().as_bytes()),
                    VersionVector::new());
            db1.response(i).unwrap();
        }
        for i in 0..TEST_JOIN_SIZE {
            db1.get(i, i.to_string().as_bytes());
            let result1 = db1.response(i);
            db2.get(i, i.to_string().as_bytes());
            let result2 = db2.response(i);
            assert_eq!(result1, result2);
        }

        // sim unclean shutdown
        drop(db2);
        let _ = fs::remove_dir_all("t/db2");
        db2 = TestDatabase::new(2, "127.0.0.1:9001".parse().unwrap(), "t/db2", false);

        thread::sleep_ms(1000);
        while db1.syncs_inflight() + db2.syncs_inflight() > 0 {
            warn!("waiting for rev syncs to finish");
            thread::sleep_ms(1000);
        }

        // force some syncs
        for i in 0..64u16 {
            db2.start_sync(i, false);
        }

        thread::sleep_ms(1000);
        while db1.syncs_inflight() + db2.syncs_inflight() > 0 {
            warn!("waiting for syncs to finish");
            thread::sleep_ms(1000);
        }

        // FIXME: this is broken until we can specify R=1
        warn!("will check data in db2 after sync");
        for i in 0..TEST_JOIN_SIZE {
            db2.get(i, i.to_string().as_bytes());
            assert!(db2.response(i).unwrap().values().eq(&[i.to_string().as_bytes()]));
        }
    }
}
