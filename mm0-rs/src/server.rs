use std::{fs, io};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}, Condvar};
use std::collections::{HashMap, HashSet, hash_map::{Entry, DefaultHasher}};
use std::hash::{Hash, Hasher};
use std::result;
use std::thread::{ThreadId, self};
use std::time::Instant;
use futures::{FutureExt, future::BoxFuture};
use futures::channel::oneshot::{Sender as FSender, channel};
use futures::executor::ThreadPool;
use futures::lock::Mutex as FMutex;
use lsp_server::*;
use serde::ser::Serialize;
use serde_json::{from_value, to_value};
use lsp_types::*;
use crossbeam::{channel::{SendError, RecvError}};
use crate::util::*;
use crate::lined_string::LinedString;
use crate::parser::{AST, parse};
use crate::elab::{ElabError, Elaborator,
  environment::{ObjectKind, DeclKey, Environment},
  local_context::InferSort, lisp::print::FormatEnv};

#[derive(Debug)]
struct ServerError(BoxError);

type Result<T> = result::Result<T, ServerError>;

impl From<serde_json::Error> for ServerError {
  fn from(e: serde_json::error::Error) -> Self { ServerError(Box::new(e)) }
}

impl From<ProtocolError> for ServerError {
  fn from(e: ProtocolError) -> Self { ServerError(Box::new(e)) }
}

impl From<RecvError> for ServerError {
  fn from(e: RecvError) -> Self { ServerError(Box::new(e)) }
}

impl<T: Send + Sync + 'static> From<SendError<T>> for ServerError {
  fn from(e: SendError<T>) -> Self { ServerError(Box::new(e)) }
}

impl From<&'static str> for ServerError {
  fn from(e: &'static str) -> Self { ServerError(e.into()) }
}

impl From<io::Error> for ServerError {
  fn from(e: io::Error) -> Self { ServerError(Box::new(e)) }
}

impl From<BoxError> for ServerError {
  fn from(e: BoxError) -> Self { ServerError(e) }
}

impl From<String> for ServerError {
  fn from(e: String) -> Self { ServerError(e.into()) }
}

fn nos_id(nos: NumberOrString) -> RequestId {
  match nos {
    NumberOrString::Number(n) => n.into(),
    NumberOrString::String(s) => s.into(),
  }
}

lazy_static! {
  static ref LOGGER: (Mutex<Vec<(Instant, ThreadId, String)>>, Condvar) = Default::default();
  static ref SERVER: Server = Server::new().expect("Initialization failed");
}
#[allow(unused)]
pub fn log(s: String) {
  LOGGER.0.lock().unwrap().push((Instant::now(), thread::current().id(), s));
  LOGGER.1.notify_one();
}

#[allow(unused)]
macro_rules! log {
  ($($es:tt)*) => {crate::server::log(format!($($es)*))}
}

async fn elaborate(path: FileRef, start: Option<Position>,
    cancel: Arc<AtomicBool>) -> Result<(u64, Arc<Environment>)> {
  let Server {vfs, pool, ..} = &*SERVER;
  let (path, file) = vfs.get_or_insert(path)?;
  let v = file.text.lock().unwrap().0;
  let (old_ast, old_env, old_deps) = {
    let mut g = file.parsed.lock().await;
    let (res, senders) = match &mut *g {
      None => ((None, None, vec![]), vec![]),
      &mut Some(FileCache::InProgress {version, ref cancel, ref mut senders}) => {
        if v == version {
          let (send, recv) = channel();
          senders.push(send);
          drop(g);
          return Ok(recv.await.unwrap())
        }
        cancel.store(true, Ordering::SeqCst);
        if let Some(FileCache::InProgress {senders, ..}) = g.take() {
          ((None, None, vec![]), senders)
        } else {unsafe {std::hint::unreachable_unchecked()}}
      }
      &mut Some(FileCache::Ready {hash, ref deps, ref env, complete, ..}) => {
        if complete && (|| -> bool {
          let hasher = &mut DefaultHasher::new();
          v.hash(hasher);
          for path in deps {
            if let Some(file) = vfs.get(path) {
              if let Some(g) = file.parsed.try_lock() {
                if let Some(FileCache::Ready {hash, ..}) = *g {
                  hash.hash(hasher);
                } else {return false}
              } else {return false}
            } else {return false}
          }
          hasher.finish() == hash
        })() {return Ok((hash, env.clone()))}
        if let Some(FileCache::Ready {ast, errors, deps, env, ..}) = g.take() {
          ((start.map(|s| (s, ast)), Some((errors, env)), deps), vec![])
        } else {unsafe {std::hint::unreachable_unchecked()}}
      }
    };
    *g = Some(FileCache::InProgress {version: v, cancel: cancel.clone(), senders});
    res
  };
  let (version, text) = file.text.lock().unwrap().clone();
  let mut hasher = DefaultHasher::new();
  version.hash(&mut hasher);
  let (idx, ast) = parse(text, old_ast);
  let ast = Arc::new(ast);

  let mut deps = Vec::new();
  let elab = Elaborator::new(ast.clone(), path.clone(), cancel.clone());
  let (toks, errors, env) = elab.as_fut(
    old_env.map(|(errs, e)| (idx, errs, e)),
    |path| {
      let path = vfs.get_or_insert(path)?.0;
      let (send, recv) = channel();
      pool.spawn_ok(elaborate_and_send(path.clone(), cancel.clone(), send));
      deps.push(path);
      Ok(recv)
    }).await;
  for tok in toks {tok.hash(&mut hasher)}
  let hash = hasher.finish();
  let env = Arc::new(env);
  log!("elabbed {:?}", path);
  let mut g = file.parsed.lock().await;
  let complete = !cancel.load(Ordering::SeqCst);
  if complete {
    let mut srcs = HashMap::new();
    let mut to_loc = |fsp: &FileSpan| -> Location {
      if fsp.file.ptr_eq(&path) {
        &ast.source
      } else {
        srcs.entry(fsp.file.ptr()).or_insert_with(||
          vfs.0.lock().unwrap().get(&fsp.file).unwrap()
            .text.lock().unwrap().1.clone())
      }.to_loc(fsp)
    };
    let errs: Vec<_> = ast.errors.iter().map(|e| e.to_diag(&ast.source))
      .chain(errors.iter().map(|e| e.to_diag(&ast.source, &mut to_loc))).collect();
    log!("diagged {:?}, {} errors", path, errs.len());
    send_diagnostics(path.url().clone(), errs)?;
  }
  vfs.update_downstream(&old_deps, &deps, &path);
  if let Some(FileCache::InProgress {senders, ..}) = g.take() {
    for s in senders {
      let _ = s.send((hash, env.clone()));
    }
  }
  *g = Some(FileCache::Ready {hash, ast, errors, deps, env: env.clone(), complete});
  drop(g);
  for d in file.downstream.lock().unwrap().iter() {
    log!("{:?} affects {:?}", path, d);
    pool.spawn_ok(dep_change(d.clone()));
  }
  Ok((hash, env))
}

async fn elaborate_and_report(path: FileRef, start: Option<Position>, cancel: Arc<AtomicBool>) {
  if let Err(e) = std::panic::AssertUnwindSafe(elaborate(path, start, cancel))
      .catch_unwind().await
      .unwrap_or_else(|_| Err("server panic".into())) {
    log_message(format!("{:?}", e).into()).unwrap();
  }
}

fn elaborate_and_send(path: FileRef,
  cancel: Arc<AtomicBool>, send: FSender<(u64, Arc<Environment>)>) ->
  BoxFuture<'static, ()> {
  async {
    if let Ok(env) = elaborate(path, Some(Position::default()), cancel).await {
      let _ = send.send(env);
    }
  }.boxed()
}

fn dep_change(path: FileRef) -> BoxFuture<'static, ()> {
  elaborate_and_report(path, None, Arc::new(AtomicBool::new(false))).boxed()
}

enum FileCache {
  InProgress {
    version: Option<i64>,
    cancel: Arc<AtomicBool>,
    senders: Vec<FSender<(u64, Arc<Environment>)>>,
  },
  Ready {
    hash: u64,
    ast: Arc<AST>,
    errors: Vec<ElabError>,
    env: Arc<Environment>,
    deps: Vec<FileRef>,
    complete: bool,
  }
}

struct VirtualFile {
  /// File data, saved (true) or unsaved (false)
  text: Mutex<(Option<i64>, Arc<LinedString>)>,
  /// File parse
  parsed: FMutex<Option<FileCache>>,
  /// Files that depend on this one
  downstream: Mutex<HashSet<FileRef>>,
}

impl VirtualFile {
  fn new(version: Option<i64>, text: String) -> VirtualFile {
    VirtualFile {
      text: Mutex::new((version, Arc::new(text.into()))),
      parsed: FMutex::new(None),
      downstream: Mutex::new(HashSet::new())
    }
  }
}

struct VFS(Mutex<HashMap<FileRef, Arc<VirtualFile>>>);

impl VFS {
  fn get(&self, path: &FileRef) -> Option<Arc<VirtualFile>> {
    self.0.lock().unwrap().get(path).cloned()
  }

  fn get_or_insert(&self, path: FileRef) -> io::Result<(FileRef, Arc<VirtualFile>)> {
    match self.0.lock().unwrap().entry(path) {
      Entry::Occupied(e) => Ok((e.key().clone(), e.get().clone())),
      Entry::Vacant(e) => {
        let path = e.key().clone();
        let s = fs::read_to_string(path.path())?;
        let val = e.insert(Arc::new(VirtualFile::new(None, s))).clone();
        Ok((path, val))
      }
    }
  }

  fn source(&self, file: &FileRef) -> Arc<LinedString> {
    self.0.lock().unwrap().get(&file).unwrap().text.lock().unwrap().1.clone()
  }

  fn open_virt(&self, path: FileRef, version: i64, text: String) -> Result<Arc<VirtualFile>> {
    let pool = &SERVER.pool;
    let file = Arc::new(VirtualFile::new(Some(version), text));
    let file = match self.0.lock().unwrap().entry(path.clone()) {
      Entry::Occupied(entry) => {
        for dep in entry.get().downstream.lock().unwrap().iter() {
          pool.spawn_ok(dep_change(dep.clone()));
        }
        file
      }
      Entry::Vacant(entry) => entry.insert(file).clone()
    };
    pool.spawn_ok(elaborate_and_report(path, Some(Position::default()),
      Arc::new(AtomicBool::new(false))));
    Ok(file)
  }

  fn close(&self, path: &FileRef) -> Result<()> {
    let mut g = self.0.lock().unwrap();
    if let Entry::Occupied(e) = g.entry(path.clone()) {
      if e.get().downstream.lock().unwrap().is_empty() {
        send_diagnostics(path.url().clone(), vec![])?;
        e.remove();
      } else if e.get().text.lock().unwrap().0.take().is_some() {
        let file = e.get().clone();
        drop(g);
        let pool = &SERVER.pool;
        for dep in file.downstream.lock().unwrap().clone() {
          pool.spawn_ok(dep_change(dep.clone()));
        }
      }
    }
    Ok(())
  }

  fn update_downstream(&self, old_deps: &[FileRef], deps: &[FileRef], to: &FileRef) {
    for from in old_deps {
      if !deps.contains(from) {
        let file = self.0.lock().unwrap().get(from).unwrap().clone();
        file.downstream.lock().unwrap().remove(to);
      }
    }
    for from in deps {
      if !old_deps.contains(from) {
        let file = self.0.lock().unwrap().get(from).unwrap().clone();
        file.downstream.lock().unwrap().insert(to.clone());
      }
    }
  }
}

enum RequestType {
  Completion(CompletionParams),
  Hover(TextDocumentPositionParams),
  Definition(TextDocumentPositionParams),
  DocumentSymbol(DocumentSymbolParams),
}

fn parse_request(req: Request) -> Result<Option<(RequestId, RequestType)>> {
  let Request {id, method, params} = req;
  match method.as_str() {
    "textDocument/completion"     => Ok(Some((id, RequestType::Completion(from_value(params)?)))),
    "textDocument/hover"          => Ok(Some((id, RequestType::Hover(from_value(params)?)))),
    "textDocument/definition"     => Ok(Some((id, RequestType::Definition(from_value(params)?)))),
    "textDocument/documentSymbol" => Ok(Some((id, RequestType::DocumentSymbol(from_value(params)?)))),
    _ => Ok(None)
  }
}

fn send_message<T: Into<Message>>(t: T) -> Result<()> {
  Ok(SERVER.conn.sender.send(t.into())?)
}

#[allow(unused)]
fn show_message(typ: MessageType, message: String) -> Result<()> {
  send_message(Notification {
    method: "window/showMessage".to_owned(),
    params: to_value(ShowMessageParams {typ, message})?
  })
}

#[allow(unused)]
fn log_message(message: String) -> Result<()> {
  send_message(Notification {
    method: "window/logMessage".to_owned(),
    params: to_value(LogMessageParams {typ: MessageType::Log, message})?
  })
}

fn send_diagnostics(uri: Url, diagnostics: Vec<Diagnostic>) -> Result<()> {
  send_message(Notification {
    method: "textDocument/publishDiagnostics".to_owned(),
    params: to_value(PublishDiagnosticsParams {uri, diagnostics})?
  })
}

type OpenRequests = Mutex<HashMap<RequestId, Arc<AtomicBool>>>;

struct RequestHandler {
  id: RequestId,
  #[allow(unused)]
  cancel: Arc<AtomicBool>,
}

impl RequestHandler {
  async fn handle(self, req: RequestType) -> Result<()> {
    match req {
      RequestType::Hover(TextDocumentPositionParams {text_document: doc, position}) =>
        self.finish(hover(FileRef::from_url(doc.uri), position).await),
      RequestType::Definition(TextDocumentPositionParams {text_document: doc, position}) =>
        if let Some(true) = SERVER.params.capabilities.text_document
          .as_ref().and_then(|d| d.definition.as_ref().and_then(|g| g.link_support)) {
          self.finish(definition(FileRef::from_url(doc.uri), position,
            |text, text2, src, &FileSpan {ref file, span}, full| LocationLink {
              origin_selection_range: Some(text.to_range(src)),
              target_uri: file.url().clone(),
              target_range: text2.to_range(full),
              target_selection_range: text2.to_range(span),
            }).await)
        } else {
          self.finish(definition(FileRef::from_url(doc.uri), position,
            |_, text2, _, &FileSpan {ref file, span}, _| Location {
              uri: file.url().clone(),
              range: text2.to_range(span),
            }).await)
        },
      _ => self.finish(Ok(()))
    }
  }

  fn finish<T: Serialize>(self, resp: result::Result<T, ResponseError>) -> Result<()> {
    let Server {reqs, conn, ..} = &*SERVER;
    reqs.lock().unwrap().remove(&self.id);
    conn.sender.send(Message::Response(match resp {
      Ok(val) => Response { id: self.id, result: Some(to_value(val)?), error: None },
      Err(e) => Response { id: self.id, result: None, error: Some(e) }
    }))?;
    Ok(())
  }
}

async fn hover(path: FileRef, pos: Position) -> result::Result<Option<Hover>, ResponseError> {
  let Server {vfs, ..} = &*SERVER;
  macro_rules! or_none {($e:expr)  => {match $e {
    Some(x) => x,
    None => return Ok(None)
  }}}
  let file = vfs.get(&path).ok_or_else(||
    response_err(ErrorCode::InvalidRequest, "hover nonexistent file"))?;
  let text = file.text.lock().unwrap().1.clone();
  let idx = or_none!(text.to_idx(pos));
  let env = elaborate(path, Some(Position::default()), Arc::new(AtomicBool::from(false)))
    .await.map_err(|e| response_err(ErrorCode::InternalError, format!("{:?}", e)))?.1;
  let fe = FormatEnv {source: &text, env: &env};
  let spans = or_none!(env.find(idx));
  let res: Vec<_> = spans.find_pos(idx).into_iter().filter_map(|&(sp, ref k)| {
    Some(match k {
      &ObjectKind::Sort(s) => (sp, format!("{}", &env.sorts[s])),
      &ObjectKind::Term(t, sp1) => (sp1, format!("{}", fe.to(&env.terms[t]))),
      &ObjectKind::Thm(t) => (sp, format!("{}", fe.to(&env.thms[t]))),
      &ObjectKind::Var(x) => (sp, match spans.lc.as_ref().and_then(|lc| lc.vars.get(&x)) {
        Some((_, InferSort::Bound {sort})) => format!("{{{}: {}}}", fe.to(&x), fe.to(sort)),
        Some((_, InferSort::Reg {sort, deps})) => {
          let mut s = format!("({}: {}", fe.to(&x), fe.to(sort));
          for &a in deps {s += " "; s += &env.data[a].name}
          s + ")"
        }
        _ => return None,
      }),
      ObjectKind::Global(_) |
      ObjectKind::Import(_) => return None,
    })
  }).collect();
  if res.is_empty() {return Ok(None)}
  Ok(Some(Hover {
    range: Some(text.to_range(res[0].0)),
    contents: HoverContents::Array(res.into_iter().map(|(_, value)|
      MarkedString::LanguageString(LanguageString {language: "mm0".into(), value})).collect())
  }))
}

async fn definition<T>(path: FileRef, pos: Position,
    f: impl Fn(&LinedString, &LinedString, Span, &FileSpan, Span) -> T) ->
    result::Result<Vec<T>, ResponseError> {
  let Server {vfs, ..} = &*SERVER;
  macro_rules! or_none {($e:expr)  => {match $e {
    Some(x) => x,
    None => return Ok(vec![])
  }}}
  let file = vfs.get(&path).ok_or_else(||
    response_err(ErrorCode::InvalidRequest, "goto definition nonexistent file"))?;
  let text = file.text.lock().unwrap().1.clone();
  let idx = or_none!(text.to_idx(pos));
  let env = elaborate(path.clone(), Some(Position::default()), Arc::new(AtomicBool::from(false)))
    .await.map_err(|e| response_err(ErrorCode::InternalError, format!("{:?}", e)))?.1;
  let spans = or_none!(env.find(idx));
  let mut res = vec![];
  for &(sp, ref k) in spans.find_pos(idx) {
    let g = |fsp: &FileSpan, full|
      if fsp.file.ptr_eq(&path) {
        f(&text, &text, sp, fsp, full)
      } else {
        f(&text, &vfs.source(&fsp.file), sp, fsp, full)
      };
    let sort = |s| {
      let sd = &env.sorts[s];
      g(&sd.span, sd.full)
    };
    let term = |t| {
      let td = &env.terms[t];
      g(&td.span, td.full)
    };
    let thm = |t| {
      let td = &env.thms[t];
      g(&td.span, td.full)
    };
    match k {
      &ObjectKind::Sort(s) => res.push(sort(s)),
      &ObjectKind::Term(t, _) => res.push(term(t)),
      &ObjectKind::Thm(t) => res.push(thm(t)),
      ObjectKind::Var(_) => {}
      &ObjectKind::Global(a) => {
        let ad = &env.data[a];
        match ad.decl {
          Some(DeclKey::Term(t)) => res.push(term(t)),
          Some(DeclKey::Thm(t)) => res.push(thm(t)),
          None => {}
        }
        if let Some(s) = ad.sort {res.push(sort(s))}
        if let Some((Some((ref fsp, full)), _)) = ad.lisp {
          res.push(g(&fsp, full))
        }
      }
      ObjectKind::Import(file) => {
        res.push(g(&FileSpan {file: file.clone(), span: 0.into()}, 0.into()))
      },
    }
  }
  Ok(res)
}

struct Server {
  conn: Connection,
  #[allow(unused)]
  params: InitializeParams,
  reqs: OpenRequests,
  vfs: VFS,
  pool: ThreadPool,
}

impl Server {
  fn new() -> Result<Server> {
    let (conn, _iot) = Connection::stdio();
    Ok(Server {
      params: from_value(conn.initialize(
        to_value(ServerCapabilities {
          text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::Incremental)),
          hover_provider: Some(true),
          // completion_provider: Some(CompletionOptions {
          //   resolve_provider: Some(true),
          //   ..Default::default()
          // }),
          definition_provider: Some(true),
          // document_symbol_provider: Some(true),
          ..Default::default()
        })?)?)?,
      conn,
      reqs: Mutex::new(HashMap::new()),
      vfs: VFS(Mutex::new(HashMap::new())),
      pool: ThreadPool::new()?
    })
  }

  fn run(&self) {
    crossbeam::scope(|s| {
      s.spawn(move |_| {
        let mut now = Instant::now();
        loop {
          for (i, id, s) in LOGGER.1.wait(LOGGER.0.lock().unwrap()).unwrap().drain(..) {
            let d = i.saturating_duration_since(now).as_millis();
            log_message(format!("[{:?}: {:?}ms] {}", id, d, s)).unwrap();
            now = i;
          }
        }
      });
      let mut count: i64 = 1;
      loop {
        match (|| -> Result<bool> {
          let Server {conn, reqs, vfs, pool, ..} = &*SERVER;
          match conn.receiver.recv()? {
            Message::Request(req) => {
              if conn.handle_shutdown(&req)? {
                return Ok(true)
              }
              if let Some((id, req)) = parse_request(req)? {
                let cancel = Arc::new(AtomicBool::new(false));
                reqs.lock().unwrap().insert(id.clone(), cancel.clone());
                pool.spawn_ok(async {
                  RequestHandler {id, cancel}.handle(req).await.unwrap()
                });
              }
            }
            Message::Response(resp) => {
              reqs.lock().unwrap().get(&resp.id).ok_or_else(|| "response to unknown request")?
                .store(true, Ordering::Relaxed);
            }
            Message::Notification(notif) => {
              match notif.method.as_str() {
                "$/cancelRequest" => {
                  let CancelParams {id} = from_value(notif.params)?;
                  if let Some(cancel) = reqs.lock().unwrap().get(&nos_id(id)) {
                    cancel.store(true, Ordering::Relaxed);
                  }
                }
                "textDocument/didOpen" => {
                  let DidOpenTextDocumentParams {text_document: doc} = from_value(notif.params)?;
                  let path = FileRef::from_url(doc.uri);
                  log!("open {:?}", path);
                  vfs.open_virt(path, doc.version, doc.text)?;
                }
                "textDocument/didChange" => {
                  let DidChangeTextDocumentParams {text_document: doc, content_changes} = from_value(notif.params)?;
                  if !content_changes.is_empty() {
                    let path = FileRef::from_url(doc.uri);
                    log!("change {:?}", path);
                    let start = {
                      let file = vfs.get(&path).ok_or("changed nonexistent file")?;
                      let (version, text) = &mut *file.text.lock().unwrap();
                      *version = Some(doc.version.unwrap_or_else(|| (count, count += 1).0));
                      let (start, s) = text.apply_changes(content_changes.into_iter());
                      *text = Arc::new(s);
                      start
                    };
                    pool.spawn_ok(elaborate_and_report(path, Some(start),
                      Arc::new(AtomicBool::new(false))));
                  }
                }
                "textDocument/didClose" => {
                  let DidCloseTextDocumentParams {text_document: doc} = from_value(notif.params)?;
                  let path = FileRef::from_url(doc.uri);
                  log!("close {:?}", path);
                  vfs.close(&path)?;
                }
                _ => {}
              }
            }
          }
          Ok(false)
        })() {
          Ok(true) => break,
          Ok(false) => {},
          Err(e) => eprintln!("Server panicked: {:?}", e)
        }
      }
    }).expect("other thread panicked")
  }
}

fn response_err(code: ErrorCode, message: impl Into<String>) -> ResponseError {
  ResponseError {code: code as i32, message: message.into(), data: None}
}

pub fn main(mut args: impl Iterator<Item=String>) {
  if args.next().map_or(false, |s| s == "--debug") {
    std::env::set_var("RUST_BACKTRACE", "1");
    use {simplelog::*, std::fs::File};
    let _ = WriteLogger::init(LevelFilter::Debug, Config::default(), File::create("lsp.log").unwrap());
  }
  log_message("started".into()).unwrap();
  SERVER.run()
}