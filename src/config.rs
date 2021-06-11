use anyhow::{anyhow, bail, Result};
use rustc_span::{
  source_map::{SourceFile, SourceMap},
  BytePos, FileName, Span, RealFileName
};
use serde::Serialize;
use std::default::Default;

#[derive(Serialize, Debug, Clone, Hash, PartialEq, Eq)]
pub struct Range {
  pub start_line: usize,
  pub start_col: usize,
  pub end_line: usize,
  pub end_col: usize,
  pub filename: String,
}

impl Range {
  pub fn line(line: usize, start: usize, end: usize) -> Range {
    Range {
      start_line: line,
      start_col: start,
      end_line: line,
      end_col: end,
      filename: "".to_owned(),
    }
  }

  pub fn substr(&self, s: &str) -> String {
    let lines = s.split("\n").collect::<Vec<_>>();
    if self.start_line != self.end_line {
      unimplemented!()
    } else {
      lines[self.start_line][self.start_col..self.end_col].to_owned()
    }
  }
}

impl Range {
  pub fn from_span(span: Span, source_map: &SourceMap) -> Result<Self> {
    let filename = source_map.span_to_filename(span);
    let filename = if let FileName::Real(RealFileName::LocalPath(filename)) = filename {
      filename.to_string_lossy().into_owned()
    } else {
      bail!("Range::from_span doesn't support {:?}", filename)
    };

    let lines = source_map
      .span_to_lines(span)
      .map_err(|e| anyhow!("{:?}", e))?;
    if lines.lines.len() == 0 {
      return Ok(Range {
        start_line: 0,
        start_col: 0,
        end_line: 0,
        end_col: 0,
        filename,
      });
    }

    let start_line = lines.lines.first().unwrap();
    let end_line = lines.lines.last().unwrap();

    Ok(Range {
      start_line: start_line.line_index,
      start_col: start_line.start_col.0,
      end_line: end_line.line_index,
      end_col: end_line.end_col.0,
      filename,
    })
  }

  pub fn to_span(&self, source_file: &SourceFile) -> Option<Span> {
    if self.end_line >= source_file.lines.len() {
      return None;
    }

    let start_pos = source_file.line_bounds(self.start_line).start + BytePos(self.start_col as u32);
    let end_pos = source_file.line_bounds(self.end_line).start + BytePos(self.end_col as u32);
    Some(Span::with_root_ctxt(start_pos, end_pos))
  }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Hash)]
pub enum MutabilityMode {
  DistinguishMut,
  IgnoreMut,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Hash)]
pub enum ContextMode {
  SigOnly,
  Recurse,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Hash)]
pub enum PointerMode {
  Precise,
  Conservative,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Hash)]
pub struct EvalMode {
  pub mutability_mode: MutabilityMode,
  pub context_mode: ContextMode,
  pub pointer_mode: PointerMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Config {
  pub range: Range,
  pub debug: bool,
  pub eval_mode: EvalMode,
  pub local: Option<usize>
}

impl Default for Config {
  fn default() -> Self {
    Config {
      range: Range::line(0, 0, 0),
      debug: false,
      eval_mode: EvalMode {
        mutability_mode: MutabilityMode::DistinguishMut,
        context_mode: ContextMode::SigOnly,
        pointer_mode: PointerMode::Precise,
      },
      local: None
    }
  }
}
