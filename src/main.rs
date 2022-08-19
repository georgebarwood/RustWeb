#![forbid(unsafe_code)]

#[tokio::main]
/// Execution starts here.
async fn main() {
    // console_subscriber::init();

    let args = Args::parse();
    println!("ip={} port={} mem={} rep={} login={}", args.ip, args.port, args.mem, args.rep, args.login);

    let listen = format!("{}:{}", args.ip, args.port);
    let listen = listen.parse().expect("Error parsing listen address:port");
    let is_master = args.rep == "";
    let replicate_source = args.rep;
    let replicate_credentials = args.login;

    // Construct an AtomicFile. This ensures that updates to the database are "all or nothing".
    let file = Box::new(SimpleFileStorage::new("rustweb.rustdb"));
    let upd = Box::new(SimpleFileStorage::new("rustweb.upd"));
    let stg = Box::new(AtomicFile::new(file, upd));

    // SharedPagedData allows for one writer and multiple readers.
    // Note that readers never have to wait, they get a "virtual" read-only copy of the database.
    let spd = Arc::new(SharedPagedData::new(stg));
    {
       let mut s = spd.stash.write().unwrap();
       s.mem_limit = args.mem * 1000000;
       s.trace = true;
    }
    // Construct map of "builtin" functions that can be called in SQL code.
    // Include extra functions ARGON, EMAILTX and SLEEP as well as the standard functions.
    let mut bmap = BuiltinMap::default();
    standard_builtins(&mut bmap);
    let list = [
        ("ARGON", DataKind::Binary, CompileFunc::Value(c_argon)),
        ("EMAILTX", DataKind::Int, CompileFunc::Int(c_email_tx)),
        ("SLEEP", DataKind::Int, CompileFunc::Int(c_sleep)),
        ("TRANSWAIT", DataKind::Int, CompileFunc::Int(c_trans_wait)),
/*
        ("BINPACK", DataKind::Binary, CompileFunc::Value(c_binpack)),
        (
            "BINUNPACK",
            DataKind::Binary,
            CompileFunc::Value(c_binunpack),
        ),
*/
    ];
    for (name, typ, cf) in list {
        bmap.insert(name.to_string(), (typ, cf));
    }
    let bmap = Arc::new(bmap);

    // Construct task communication channels.
    let (tx, mut rx) = mpsc::channel::<ServerMessage>(1);
    let (email_tx, email_rx) = mpsc::unbounded_channel::<()>();
    let (sleep_tx, sleep_rx) = mpsc::unbounded_channel::<u64>();
    let (sync_tx, sync_rx) = oneshot::channel::<bool>();
    let (wait_tx, _wait_rx) = broadcast::channel::<()>(16);

    // Construct shared state.
    let ss = Arc::new(SharedState {
        spd: spd.clone(),
        bmap: bmap.clone(),
        tx,
        email_tx,
        sleep_tx,
        wait_tx,
        is_master,
        replicate_source,
        replicate_credentials,
    });

    if is_master {
        // Start the email task.
        let ssc = ss.clone();
        tokio::spawn(async move { email_loop(email_rx, ssc).await });

        // Start the sleep task.
        let ssc = ss.clone();
        tokio::spawn(async move { sleep_loop(sleep_rx, ssc).await });
    } else {
        // Start the sync task.
        let ssc = ss.clone();
        tokio::spawn(async move { sync_loop(sync_rx, ssc).await });
    }

    // Start the task that updates the database.
    let ssc = ss.clone();
    thread::spawn(move || {
        let ss = ssc;

        // Get write-access to database ( there will only be one of these ).
        let wapd = AccessPagedData::new_writer(spd);

        let db = Database::new(wapd, if is_master { init::INITSQL } else { "" }, bmap);
        if !is_master {
            let _ = sync_tx.send(db.is_new);
        }
        loop {
            let mut sm = rx.blocking_recv().unwrap();
            let sql = sm.st.x.qy.sql.clone();
            db.run_timed(&sql, &mut *sm.st.x);

            if sm.st.log && db.changed() {
                if let Some(t) = db.get_table(&ObjRef::new("log", "Transaction")) {
                    // Append serialised transaction to log.Transaction table
                    let ser = rmp_serde::to_vec(&sm.st.x.qy).unwrap();
                    let ser = Value::RcBinary(Rc::new(ser));
                    let mut row = t.row();
                    row.id = t.alloc_id() as i64;
                    row.values[0] = ser;
                    t.insert(&db, &mut row);
                }
            }
            let updates = db.save();
            if updates > 0 {
                let _ = ss.wait_tx.send(());
                println!("Pages updated={updates}");
            }
            let _x = sm.reply.send(sm.st);

            ss.trim_cache();
        }
    });

    // Build the axum app with a single route.
    let app = Router::new().route("/*key", get(h_get).post(h_post)).layer(
        ServiceBuilder::new()
            .layer(CookieManagerLayer::new())
            .layer(Extension(ss.clone())),
    );

    // Run the axum app.
    axum::Server::bind(&listen)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

/// Database initialisation string.
mod init;

use mimalloc::MiMalloc;

/// Memory allocator ( MiMalloc ).
#[global_allocator]
static MEMALLOC: MiMalloc = MiMalloc;

use axum::{
    extract::{Extension, Form, Multipart, Path, Query},
    routing::get,
    Router,
};
use rustdb::{
    c_int, c_value, check_types, standard_builtins, AccessPagedData, AtomicFile, Block, BuiltinMap,
    CExp, CExpPtr, CompileFunc, DataKind, Database, EvalEnv, Expr, GenTransaction, ObjRef, Part,
    SharedPagedData, SimpleFileStorage, Transaction, Value,
};
use std::{collections::BTreeMap, rc::Rc, sync::Arc, thread};

use tokio::sync::{broadcast, mpsc, oneshot};
use tower::ServiceBuilder;
use tower_cookies::{CookieManagerLayer, Cookies};

/// Transaction to be sent to server task, implements IntoResponse.
struct ServerTrans {
    x: Box<GenTransaction>,
    log: bool,
}

impl ServerTrans {
    fn new() -> Self {
        let mut result = Self {
            x: Box::new(GenTransaction::new()),
            log: true,
        };
        result.x.ext = TransExt::new();
        result
    }
}

/// Message to server task, includes oneshot Sender for reply.
struct ServerMessage {
    st: ServerTrans,
    reply: oneshot::Sender<ServerTrans>,
}

/// Extra transaction data.
#[derive(Default)]
struct TransExt {
    /// Signals there is new email to be sent.
    tx_email: bool,
    /// Signals time to sleep.
    sleep: u64,
    /// Signals wait for new transaction to be logged
    trans_wait: bool,
}

impl TransExt {
    fn new() -> Box<Self> {
        Box::new(Self::default())
    }
}

/// State shared with handlers.
struct SharedState {
    /// Shared storage used for read-only queries.
    spd: Arc<SharedPagedData>,
    /// Map of builtin SQL functions for Database.
    bmap: Arc<BuiltinMap>,
    /// Sender channel for sending queries to server task.
    tx: mpsc::Sender<ServerMessage>,
    /// For notifying email loop that emails are in Queue ready to be sent.
    email_tx: mpsc::UnboundedSender<()>,
    /// For setting sleep time.
    sleep_tx: mpsc::UnboundedSender<u64>,
    /// For notifying tasks waiting for transaction.
    wait_tx: broadcast::Sender<()>,
    /// Server is master ( not replicating another database ).
    is_master: bool,
    replicate_source: String,
    replicate_credentials: String,
}

impl SharedState {
    async fn process(&self, st: ServerTrans) -> ServerTrans {
        let (reply, rx) = oneshot::channel::<ServerTrans>();
        let _err = self.tx.send(ServerMessage { st, reply }).await;
        let mut st = rx.await.unwrap();
        if self.is_master {
            // Check if email needs sending or sleep time has been specified, etc.
            let ext = st.x.get_extension();
            if let Some(ext) = ext.downcast_ref::<TransExt>() {
                if ext.sleep > 0 {
                    let _ = self.sleep_tx.send(ext.sleep);
                }
                if ext.tx_email {
                    let _ = self.email_tx.send(());
                }
            }
        }
        st
    }

    fn trim_cache(&self) {
        self.spd.trim_cache();
    }
}

/// Handler for http GET requests.
async fn h_get(
    state: Extension<Arc<SharedState>>,
    path: Path<String>,
    params: Query<BTreeMap<String, String>>,
    cookies: Cookies,
) -> ServerTrans {
    // Build the ServerTrans.
    let mut st = ServerTrans::new();
    st.x.qy.path = path.0;
    st.x.qy.params = params.0;
    st.x.qy.cookies = map_cookies(cookies);

    let mut wait_rx = state.wait_tx.subscribe();
    let spd = state.spd.clone();
    let bmap = state.bmap.clone();

    let mut st = tokio::task::spawn_blocking(move || {
        // GET requests should be read-only.
        let apd = AccessPagedData::new_reader(spd);
        let db = Database::new(apd, "", bmap);
        let sql = st.x.qy.sql.clone();
        db.run_timed(&sql, &mut *st.x);
        st
    })
    .await
    .unwrap();

    let ext = st.x.get_extension();
    if let Some(ext) = ext.downcast_ref::<TransExt>() {
        if ext.trans_wait {
            tokio::select! {
               _ = wait_rx.recv() => {}
               _ = tokio::time::sleep(core::time::Duration::from_secs(600)) => {}
            }
        }
    }
    state.trim_cache();
    st
}

/// Handler for http POST requests.
async fn h_post(
    state: Extension<Arc<SharedState>>,
    path: Path<String>,
    params: Query<BTreeMap<String, String>>,
    cookies: Cookies,
    form: Option<Form<BTreeMap<String, String>>>,
    multipart: Option<Multipart>,
) -> ServerTrans {
    // Build the Server Transaction.
    let mut st = ServerTrans::new();
/*
    if !state.is_master {
        st.x.rp.status_code = 421; // Misdirected Request
        return st;
    }
*/
    st.x.qy.path = path.0;
    st.x.qy.params = params.0;
    st.x.qy.cookies = map_cookies(cookies);
    if let Some(Form(form)) = form {
        st.x.qy.form = form;
    } else {
        st.x.qy.parts = map_parts(multipart).await;
    }
    // Process the Server Transaction.
    state.process(st).await
}

use axum::{
    body::{boxed, BoxBody, Full},
    http::{header::HeaderName, status::StatusCode, HeaderValue, Response},
    response::IntoResponse,
};

impl IntoResponse for ServerTrans {
    fn into_response(self) -> Response<BoxBody> {
        let bf = boxed(Full::from(self.x.rp.output));
        let mut res = Response::builder().body(bf).unwrap();

        *res.status_mut() = StatusCode::from_u16(self.x.rp.status_code).unwrap();

        for (name, value) in &self.x.rp.headers {
            res.headers_mut().append(
                HeaderName::from_lowercase(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        res
    }
}

/// task for syncing with master database
async fn sync_loop(rx: oneshot::Receiver<bool>, state: Arc<SharedState>) {
    let db_is_new = rx.await.unwrap();
    if db_is_new {
        let sql = rget(state.clone(), "/ScriptExact").await;
        let sql = std::str::from_utf8(&sql).unwrap().to_string();
        let mut st = ServerTrans::new();
        st.log = false;
        st.x.qy.sql = Arc::new(sql);
        state.process(st).await;
        println!("New slave database initialised");
    }
    loop {
        let tid = {
            let apd = AccessPagedData::new_reader(state.spd.clone());
            let db = Database::new(apd, "", state.bmap.clone());
            let lt = db.table("log", "Transaction");
            lt.id_gen.get()
        };
        let url = format!("/GetTransaction?k={tid}");
        let ser = rget(state.clone(), &url).await;
        if !ser.is_empty() {
            let mut st = ServerTrans::new();
            st.x.qy = rmp_serde::from_slice(&ser).unwrap();
            state.process(st).await;
            println!("Slave database updated Transaction Id={tid}");
        }
    }
}

/// Sleep function that checks real time elapsed.
async fn sleep_real(secs: u64) {
    let start = std::time::SystemTime::now();
    for _ in (0..secs).step_by(10) {
        tokio::time::sleep(core::time::Duration::from_secs(10)).await;
        match start.elapsed() {
            Ok(e) => {
                if e >= core::time::Duration::from_secs(secs) {
                    return;
                }
            }
            Err(_) => {
                return;
            }
        }
    }
}

/// Get data from master server, retries in case of error.
async fn rget(state: Arc<SharedState>, query: &str) -> Vec<u8> {
    // get a client builder
    let client = reqwest::Client::builder()
        .default_headers(reqwest::header::HeaderMap::new())
        .build()
        .unwrap();
    loop {
        let mut retry_delay = true;
        let req = client
            .get(state.replicate_source.clone() + query)
            .header("Cookie", state.replicate_credentials.clone());

        tokio::select! {
            response = req.send() =>
            {
                match response
                {
                  Ok(r) => {
                     let status = r.status();
                     if status.is_success()
                     {
                         match r.bytes().await {
                            Ok(b) => { return b.to_vec(); }
                            Err(e) => { println!("rget failed to get bytes err={e}" ); }
                         }
                     } else {
                         println!("rget bad response status = {status}");
                     }
                  }
                  Err(e) => {
                    println!("rget send error {e}");
                  }
               }
            }
            _ = sleep_real(800) =>
            {
              println!( "rget timed out after 800 seconds" );
              retry_delay = false;
            }
        }
        if retry_delay {
            // Wait before retrying after error/timeout.
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        }
    }
}

/// task for sleeping - calls timed.Run once sleep time has elapsed.
async fn sleep_loop(mut rx: mpsc::UnboundedReceiver<u64>, state: Arc<SharedState>) {
    let mut sleep_micro = 5000000;
    loop {
        tokio::select! {
            ns = rx.recv() => { sleep_micro = ns.unwrap(); }
            _ = tokio::time::sleep(core::time::Duration::from_micros(sleep_micro)) =>
            {
              if state.is_master
              {
                let mut st = ServerTrans::new();
                st.x.qy.sql = Arc::new("EXEC timed.Run()".to_string());
                state.process(st).await;
              }
            }
        }
    }
}

/// task that sends emails
async fn email_loop(mut rx: mpsc::UnboundedReceiver<()>, state: Arc<SharedState>) {
    loop {
        let mut send_list = Vec::new();
        {
            let _ = rx.recv().await;
            let apd = AccessPagedData::new_reader(state.spd.clone());
            let db = Database::new(apd, "", state.bmap.clone());
            let qt = db.table("email", "Queue");
            let mt = db.table("email", "Msg");
            let at = db.table("email", "SmtpAccount");

            for (pp, off) in qt.scan(&db) {
                let p = &pp.borrow();
                let a = qt.access(p, off);
                let msg = a.int(0) as u64;

                if let Some((pp, off)) = mt.id_get(&db, msg) {
                    let p = &pp.borrow();
                    let a = mt.access(p, off);
                    let from = a.str(&db, 0);
                    let to = a.str(&db, 1);
                    let title = a.str(&db, 2);
                    let body = a.str(&db, 3);
                    let format = a.int(4);
                    let account = a.int(5) as u64;

                    if let Some((pp, off)) = at.id_get(&db, account) {
                        let p = &pp.borrow();
                        let a = at.access(p, off);
                        let server = a.str(&db, 0);
                        let username = a.str(&db, 1);
                        let password = a.str(&db, 2);

                        send_list.push((
                            msg,
                            (from, to, title, body, format),
                            (server, username, password),
                        ));
                    }
                }
            }
        }
        for (msg, email, account) in send_list {
            let blocking_task = tokio::task::spawn_blocking(move || send_email(email, account));
            let result = blocking_task.await.unwrap();
            match result {
                Ok(_) => email_sent(&state, msg).await,
                Err(e) => match e {
                    EmailError::Address(ae) => {
                        email_error(&state, msg, 0, ae.to_string()).await;
                    }
                    EmailError::Lettre(le) => {
                        email_error(&state, msg, 0, le.to_string()).await;
                    }
                    EmailError::Send(se) => {
                        let retry = if se.is_transient() { 1 } else { 0 };
                        email_error(&state, msg, retry, se.to_string()).await;
                    }
                },
            }
        }
    }
}

/// Error enum for send_email
#[derive(Debug)]
enum EmailError {
    Address(lettre::address::AddressError),
    Lettre(lettre::error::Error),
    Send(lettre::transport::smtp::Error),
}

impl From<lettre::address::AddressError> for EmailError {
    fn from(e: lettre::address::AddressError) -> Self {
        EmailError::Address(e)
    }
}

impl From<lettre::error::Error> for EmailError {
    fn from(e: lettre::error::Error) -> Self {
        EmailError::Lettre(e)
    }
}

impl From<lettre::transport::smtp::Error> for EmailError {
    fn from(e: lettre::transport::smtp::Error) -> Self {
        EmailError::Send(e)
    }
}

/// Send an email using lettre.
fn send_email(
    (from, to, title, body, format): (String, String, String, String, i64),
    (server, username, password): (String, String, String),
) -> Result<(), EmailError> {
    use lettre::{
        message::SinglePart,
        transport::smtp::{
            authentication::{Credentials, Mechanism},
            PoolConfig,
        },
        Message, SmtpTransport, Transport,
    };

    let body = match format {
        1 => SinglePart::html(body),
        _ => SinglePart::plain(body),
    };

    let email = Message::builder()
        .to(to.parse()?)
        .from(from.parse()?)
        .subject(title)
        .singlepart(body)?;

    // Create TLS transport on port 587 with STARTTLS
    let sender = SmtpTransport::starttls_relay(&server)?
        // Add credentials for authentication
        .credentials(Credentials::new(username, password))
        // Configure expected authentication mechanism
        .authentication(vec![Mechanism::Plain])
        // Connection pool settings
        .pool_config(PoolConfig::new().max_size(20))
        .build();

    let _result = sender.send(&email)?;
    Ok(())
}

/// Update the database to reflect an email was sent.
async fn email_sent(state: &SharedState, msg: u64) {
    let mut st = ServerTrans::new();
    st.x.qy.sql = Arc::new(format!("EXEC email.Sent({})", msg));
    state.process(st).await;
}

/// Update the database to reflect an error occurred sending an email.
async fn email_error(state: &SharedState, msg: u64, retry: i8, err: String) {
    let mut st = ServerTrans::new();
    let src = format!("EXEC email.LogSendError({},{},'{}')", msg, retry, err);
    st.x.qy.sql = Arc::new(src);
    state.process(st).await;
}

/////////////////////////////////////////////
// Helper functions for building ServerTrans.

/// Get BTreeMap of cookies from Cookies.
fn map_cookies(cookies: Cookies) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for cookie in cookies.list() {
        let (name, value) = cookie.name_value();
        result.insert(name.to_string(), value.to_string());
    }
    result
}

/// Get Vec of Parts from MultiPart.
async fn map_parts(mp: Option<Multipart>) -> Vec<Part> {
    let mut result = Vec::new();
    if let Some(mut mp) = mp {
        while let Some(field) = mp.next_field().await.unwrap() {
            let name = field.name().unwrap().to_string();
            let file_name = match field.file_name() {
                Some(s) => s.to_string(),
                None => "".to_string(),
            };
            let content_type = match field.content_type() {
                Some(s) => s.to_string(),
                None => "".to_string(),
            };
            let mut data = Vec::new();
            let mut text = "".to_string();
            if content_type.is_empty() {
                if let Ok(s) = field.text().await {
                    text = s;
                }
            } else if let Ok(bytes) = field.bytes().await {
                data = bytes.to_vec()
            }
            let mut part = Part::default();
            part.name = name;
            part.file_name = file_name;
            part.content_type = content_type;
            part.data = Arc::new(data);
            part.text = text;
            result.push(part);
        }
    }
    result
}

/////////////////////////////

use argon2rs::argon2i_simple;

/// Compile call to ARGON.
fn c_argon(b: &Block, args: &mut [Expr]) -> CExpPtr<Value> {
    check_types(b, args, &[DataKind::String, DataKind::String]);
    let password = c_value(b, &mut args[0]);
    let salt = c_value(b, &mut args[1]);
    Box::new(Argon { password, salt })
}

/// Compiled call to ARGON.
struct Argon {
    password: CExpPtr<Value>,
    salt: CExpPtr<Value>,
}
impl CExp<Value> for Argon {
    fn eval(&self, ee: &mut EvalEnv, d: &[u8]) -> Value {
        let pw = self.password.eval(ee, d).str();
        let salt = self.salt.eval(ee, d).str();

        let result = argon2i_simple(&pw, &salt).to_vec();
        Value::RcBinary(Rc::new(result))
    }
}

/// Compile call to SLEEP.
fn c_sleep(b: &Block, args: &mut [Expr]) -> CExpPtr<i64> {
    check_types(b, args, &[DataKind::Int]);
    let to = c_int(b, &mut args[0]);
    Box::new(Sleep { to })
}

/// Compiled call to SLEEP
struct Sleep {
    to: CExpPtr<i64>,
}
impl CExp<i64> for Sleep {
    fn eval(&self, ee: &mut EvalEnv, d: &[u8]) -> i64 {
        let to = self.to.eval(ee, d);
        let mut ext = ee.tr.get_extension();
        if let Some(mut ext) = ext.downcast_mut::<TransExt>() {
            ext.sleep = if to <= 0 { 1 } else { to as u64 };
        }
        ee.tr.set_extension(ext);
        0
    }
}

/// Compile call to EMAILTX.
fn c_email_tx(b: &Block, args: &mut [Expr]) -> CExpPtr<i64> {
    check_types(b, args, &[]);
    Box::new(EmailTx {})
}

/// Compiled call to EMAILTX
struct EmailTx {}
impl CExp<i64> for EmailTx {
    fn eval(&self, ee: &mut EvalEnv, _d: &[u8]) -> i64 {
        let mut ext = ee.tr.get_extension();
        if let Some(mut ext) = ext.downcast_mut::<TransExt>() {
            ext.tx_email = true;
        }
        ee.tr.set_extension(ext);
        0
    }
}

/// Compile call to TRANSWAIT.
fn c_trans_wait(b: &Block, args: &mut [Expr]) -> CExpPtr<i64> {
    check_types(b, args, &[]);
    Box::new(TransWait {})
}

/// Compiled call to TRANSWAIT
struct TransWait {}
impl CExp<i64> for TransWait {
    fn eval(&self, ee: &mut EvalEnv, _d: &[u8]) -> i64 {
        let mut ext = ee.tr.get_extension();
        if let Some(mut ext) = ext.downcast_mut::<TransExt>() {
            ext.trans_wait = true;
        }
        ee.tr.set_extension(ext);
        0
    }
}

/*
/// Compile call to BINPACK.
fn c_binpack(b: &Block, args: &mut [Expr]) -> CExpPtr<Value> {
    check_types(b, args, &[DataKind::Binary]);
    let bytes = c_value(b, &mut args[0]);
    Box::new(Binpack { bytes })
}

/// Compiled call to BINPACK.
struct Binpack {
    bytes: CExpPtr<Value>,
}
impl CExp<Value> for Binpack {
    fn eval(&self, ee: &mut EvalEnv, d: &[u8]) -> Value {
        if let Value::RcBinary(data) = self.bytes.eval(ee, d) {
            let mut comp = flate3::Compressor::new();
            let cb: Vec<u8> = comp.deflate(&data);
            Value::RcBinary(Rc::new(cb))
        } else {
            panic!();
        }
    }
}

/// Compile call to BINUNPACK.
fn c_binunpack(b: &Block, args: &mut [Expr]) -> CExpPtr<Value> {
    check_types(b, args, &[DataKind::Binary]);
    let bytes = c_value(b, &mut args[0]);
    Box::new(Binunpack { bytes })
}

/// Compiled call to BINUNPACK.
struct Binunpack {
    bytes: CExpPtr<Value>,
}
impl CExp<Value> for Binunpack {
    fn eval(&self, ee: &mut EvalEnv, d: &[u8]) -> Value {
        if let Value::RcBinary(data) = self.bytes.eval(ee, d) {
            let ucb: Vec<u8> = flate3::inflate(&data);
            Value::RcBinary(Rc::new(ucb))
        } else {
            panic!();
        }
    }
}
*/

use clap::Parser;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Args {
   /// Port to listen on
   #[clap(value_parser = clap::value_parser!(u16).range(1..))]
   port: u16,

   /// Ip Address to listen on
   #[clap(short, long, value_parser, default_value = "0.0.0.0")]
   ip: String,

   /// Memory limit for page cache (in MB)
   #[clap(short, long, value_parser, default_value_t = 10)]
   mem: usize,

   /// Server to replicate
   #[clap(short, long, value_parser, default_value = "")]
   rep: String,

   /// Login cookies for replication
   #[clap(short, long, value_parser, default_value = "")]
   login: String,
}
