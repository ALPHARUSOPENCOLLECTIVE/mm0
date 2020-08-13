//! Join MM1/MM0 files with imports by concatenation
//!
//! This module implements a very simple import-by-text-inclusion method to allow us to have
//! `import` statements in MM0 files, even though the `import` command is not officially part
//! of MM0 and is not supported by the `mm0-c` verifier. This is essentially the same as
//! the textual inclusion used by the C `#include` preprocessor directive. In order to use
//! a file like [`mm0.mm0`], which is an MM0 file with an `import`, you have to first call
//!
//!     mm0-rs join mm0.mm0 mm0_join.mm0
//!
//! and it will create `mm0_join.mm0` by inserting the text of `peano.mm0` at the location
//! of the `import "peano.mm0";` statement. The resulting file will be a proper MM0 file and
//! can be run through the `mm0-c` verifier and other conforming verifiers.
//!
//! [`mm0.mm0`]: https://github.com/digama0/mm0/blob/master/examples/mm0.mm0
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use clap::ArgMatches;
use crate::util::FileRef;
use crate::lined_string::LinedString;
use crate::parser::{parse, ast::StmtKind};

/// Running data for the file join process.
struct Joiner<W: Write> {
  /// The current stack of file references, to reify the recursive process of
  /// following `import` directives. This lets us check for import cycles.
  stack: Vec<FileRef>,
  /// The set of files that have already been output. A file that has been output
  /// will not be printed again if another `import` for the same file is declared.
  /// This means that in a diamond dependence `A -> {B, C} -> D`, `A` will not be
  /// printed twice (once before `B` and once before `C`).
  done: HashSet<FileRef>,
  /// The writer to print the output file to
  w: W,
}

impl<W: Write> Joiner<W> {
  /// Create a new `Joiner` from a writer.
  fn new(w: W) -> Self { Self {stack: vec![], done: HashSet::new(), w} }

  /// Write the file at `path` to `self.w`, following all imports recursively.
  fn write(&mut self, path: FileRef) -> io::Result<()> {
    if let Some(i) = self.stack.iter().rposition(|x| x == &path) {
      self.stack.push(path);
      panic!("import cycle: {:?}", &self.stack[i..])
    }
    self.stack.push(path.clone());
    let src = Arc::<LinedString>::new(fs::read_to_string(path.path())?.into());
    let (_, ast) = parse(src.clone(), None);
    let mut start = 0;
    for s in &ast.stmts {
      if let StmtKind::Import(_, f) = &s.k {
        let r = FileRef::new(path.path().parent()
          .map_or_else(|| PathBuf::from(f), |p| p.join(f))
          .canonicalize()?);
        self.w.write_all(&src.as_bytes()[start..s.span.start])?;
        if self.done.insert(r.clone()) {
          self.write(r)?;
          self.w.write(&[b'\n'])?;
        }
        start = s.span.end;
      }
    }
    write!(self.w, "{}\n-- {} --\n{0}\n",
      // Safety: '-' is utf8
      unsafe { String::from_utf8_unchecked(vec![b'-'; path.rel().len() + 6]) },
      path.rel())?;
    self.w.write_all(&src.as_bytes()[start..])?;
    self.stack.pop();
    Ok(())
  }
}

/// Main entry point for `mm0-rs join` subcommand.
///
/// See the [module documentation] for the purpose of this command.
///
/// # Arguments
///
/// `mm0-rs join <in.mm0> [out.mm0]`, where:
///
/// - `in.mm0` (or `in.mm1`) is the file to join, an MM0 file with `import`s
/// - `out.mm0` is the output location, or stdin if omitted.
///
/// [module documentation]: index.html
pub fn main(args: &ArgMatches<'_>) -> io::Result<()> {
  let path = args.value_of("INPUT").unwrap();
  let file = FileRef::new(fs::canonicalize(path)?);
  match args.value_of("OUTPUT") {
    None => Joiner::new(io::stdout()).write(file),
    Some(out) => Joiner::new(fs::File::create(out)?).write(file),
  }
}