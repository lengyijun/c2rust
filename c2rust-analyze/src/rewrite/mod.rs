//! The overall implementation strategy for rewriting is:
//!
//! 1. Using the pointer permissions and flags inferred by the analysis, annotate MIR statements
//!    with the desired rewrites. These MIR-level rewrites are abstract changes to MIR statements,
//!    such as adding a cast to a particular assignment statement. This is defined in the
//!    `rewrite::expr::mir_op` module.
//!
//! 2. For each HIR expression, look at the MIR statements generated from this HIR expression and
//!    lift any MIR rewrites into HIR rewrites. HIR rewrites are expressed as concrete operations
//!    on source code, such as replacing an expression with one of its subexpressions (both
//!    identified by their `Span`s) or wrapping an expression in a ref or deref operation. The
//!    HIR-level rewrite type is `rewrite::Rewrite`; the `rewrite::expr::hir_op` module implements
//!    the lifting.
//!
//! 3. Apply the rewrites to the source code of the input program. This reads the source of each
//!    file and emits a new string consisting of the file source with certain `Span`s rewritten as
//!    specified by the HIR rewrites. The code for this is in `rewrite::apply`.
//!
//! This covers rewriting of expressions; rewriting of types is similar but mostly skips step 1,
//! since an abstract description of the changes to be made can be obtained by inspecting the
//! pointer permissions and flags directly. This code is in `rewrite::ty`. All type and expr
//! rewrites are collected and applied in one pass in step 3 (as rewriting in two passes would
//! require us to update the `Span`s mentioned in the later rewrites to account for the changes in
//! the source code produced by the earlier ones).

use rustc_hir::Mutability;
use rustc_middle::mir::Body;
use rustc_middle::mir::Location;
use rustc_middle::ty::TyCtxt;
use rustc_span::Span;
use rustc_span::source_map::FileName;
use std::fmt;
use std::fs;

mod apply;
mod expr;
mod span_index;
mod statics;
mod ty;

pub use self::expr::gen_expr_rewrites;
use self::span_index::SpanIndex;
pub use self::statics::gen_static_rewrites;
pub use self::ty::dump_rewritten_local_tys;
pub use self::ty::gen_ty_rewrites;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Rewrite<S = Span> {
    /// Take the original expression unchanged.
    Identity,
    /// Extract the subexpression at the given index.
    Sub(usize, S),

    // Expression builders
    /// `&e`, `&mut e`
    Ref(Box<Rewrite>, Mutability),
    /// `core::ptr::addr_of!(e)`, `core::ptr::addr_of_mut!(e)`
    AddrOf(Box<Rewrite>, Mutability),
    /// `*e`
    Deref(Box<Rewrite>),
    /// `arr[idx]`
    Index(Box<Rewrite>, Box<Rewrite>),
    /// `arr[idx..]`
    SliceTail(Box<Rewrite>, Box<Rewrite>),
    /// `e as T`
    Cast(Box<Rewrite>, String),
    /// The integer literal `0`.
    LitZero,
    // Function calls
    Call(String, Vec<Rewrite>),
    // Method calls
    MethodCall(String, Box<Rewrite>, Vec<Rewrite>),

    // Type builders
    /// Emit a complete pretty-printed type, discarding the original annotation.
    PrintTy(String),
    /// `*const T`, `*mut T`
    TyPtr(Box<Rewrite>, Mutability),
    /// `&T`, `&mut T`
    TyRef(Box<Rewrite>, Mutability),
    /// `[T]`
    TySlice(Box<Rewrite>),
    /// `Foo<T1, T2>`
    TyCtor(String, Vec<Rewrite>),

    // `static` builders
    /// `static` mutability (`static` <-> `static mut`)
    StaticMut(Mutability, S),
}

impl Rewrite {
    /// Pretty-print this `Rewrite` into the provided [`fmt::Formatter`].  This output format is
    /// designed solely for debugging, but generally tries to match valid Rust syntax.
    ///
    /// `prec` is the precedence of the surrounding context.  Each operatior is assigned a
    /// precedence number, where a higher precedence number means the operator binds more tightly.
    /// For example, `a + b * c` parses as `a + (b * c)`, not `(a + b) * c`, because `*` binds more
    /// tightly than `+`; this means `*` will have a higher precedence number than `+`.  Nesting a
    /// lower-precedence operator inside a higher one requires parentheses, but nesting higher
    /// precedence inside lower does not.  For example, when emitting `y + z` in the context `x *
    /// _`, we must parenthesize because `+` has lower precedence than `*`, so the result is `x *
    /// (y + z)`.  But when emitting `y * z` in the context `x + _`, we don't need to parenthesize,
    /// and the result is `x + y * z`.
    ///
    /// The `Display` impl for `Rewrite` calls `pretty` with a `prec` of 0, meaning any operator
    /// can be used without parenthesization.  Recursive calls within `pretty` will use a different
    /// `prec` as appropriate for the context.
    fn pretty(&self, f: &mut fmt::Formatter, prec: usize) -> fmt::Result {
        fn parenthesize_if(
            cond: bool,
            f: &mut fmt::Formatter,
            inner: impl FnOnce(&mut fmt::Formatter) -> fmt::Result,
        ) -> fmt::Result {
            if cond {
                f.write_str("(")?;
            }
            inner(f)?;
            if cond {
                f.write_str(")")?;
            }
            Ok(())
        }

        // Expr precedence:
        // - Index, SliceTail: 3
        // - Ref, Deref: 2
        // - Cast: 1
        //
        // Currently, we don't have any type builders that require parenthesization.

        match *self {
            // We use placeholders `$e` and `$0`, `$1`, ... for these, since the expression to be
            // rewritten is not available here.
            Rewrite::Identity => write!(f, "$e"),
            Rewrite::Sub(i, _) => write!(f, "${}", i),

            Rewrite::Ref(ref rw, mutbl) => parenthesize_if(prec > 2, f, |f| {
                match mutbl {
                    Mutability::Not => write!(f, "&")?,
                    Mutability::Mut => write!(f, "&mut ")?,
                }
                rw.pretty(f, 2)
            }),
            Rewrite::AddrOf(ref rw, mutbl) => {
                match mutbl {
                    Mutability::Not => write!(f, "core::ptr::addr_of!")?,
                    Mutability::Mut => write!(f, "core::ptr::addr_of_mut!")?,
                }
                f.write_str("(")?;
                rw.pretty(f, 0)?;
                f.write_str(")")
            }
            Rewrite::Deref(ref rw) => parenthesize_if(prec > 2, f, |f| {
                write!(f, "*")?;
                rw.pretty(f, 2)
            }),
            Rewrite::Index(ref arr, ref idx) => parenthesize_if(prec > 3, f, |f| {
                arr.pretty(f, 3)?;
                write!(f, "[")?;
                idx.pretty(f, 0)?;
                write!(f, "]")
            }),
            Rewrite::SliceTail(ref arr, ref idx) => parenthesize_if(prec > 3, f, |f| {
                arr.pretty(f, 3)?;
                write!(f, "[")?;
                // Rather than figure out the right precedence for `..`, just force
                // parenthesization in this position.
                idx.pretty(f, 999)?;
                write!(f, " ..]")
            }),
            Rewrite::Cast(ref rw, ref ty) => parenthesize_if(prec > 1, f, |f| {
                rw.pretty(f, 1)?;
                write!(f, " as {}", ty)
            }),
            Rewrite::LitZero => write!(f, "0"),

            Rewrite::PrintTy(ref s) => {
                write!(f, "{}", s)
            }
            Rewrite::Call(ref func, ref arg_rws) => {
                f.write_str(func)?;
                f.write_str("(")?;
                for (index, rw) in arg_rws.iter().enumerate() {
                    rw.pretty(f, 0)?;
                    if index < arg_rws.len() - 1 {
                        f.write_str(",")?;
                    }
                }
                f.write_str(")")
            }
            Rewrite::MethodCall(ref method, ref receiver_rw, ref arg_rws) => {
                receiver_rw.pretty(f, 0)?;
                f.write_str(".")?;
                f.write_str(method)?;
                f.write_str("(")?;
                for (index, rw) in arg_rws.iter().enumerate() {
                    rw.pretty(f, 0)?;
                    if index < arg_rws.len() - 1 {
                        f.write_str(",")?;
                    }
                }
                f.write_str(")")
            }
            Rewrite::TyPtr(ref rw, mutbl) => {
                match mutbl {
                    Mutability::Not => write!(f, "*const ")?,
                    Mutability::Mut => write!(f, "*mut ")?,
                }
                rw.pretty(f, 0)
            }
            Rewrite::TyRef(ref rw, mutbl) => {
                match mutbl {
                    Mutability::Not => write!(f, "&")?,
                    Mutability::Mut => write!(f, "&mut ")?,
                }
                rw.pretty(f, 0)
            }
            Rewrite::TySlice(ref rw) => {
                write!(f, "[")?;
                rw.pretty(f, 0)?;
                write!(f, "]")
            }
            Rewrite::TyCtor(ref name, ref rws) => {
                write!(f, "{}<", name)?;
                for rw in rws {
                    rw.pretty(f, 0)?;
                }
                write!(f, ">")
            }

            Rewrite::StaticMut(mutbl, _) => {
                match mutbl {
                    Mutability::Not => write!(f, "static (-mut) ")?,
                    Mutability::Mut => write!(f, "static (+mut) ")?,
                }
                write!(f, "$s")
            }
        }
    }
}

impl fmt::Display for Rewrite {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.pretty(f, 0)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum SoleLocationError {
    NoMatch,
    MultiMatch(Vec<Location>),
}

fn build_span_index(mir: &Body<'_>) -> SpanIndex<Location> {
    let mut span_index_items = Vec::new();
    for (bb, bb_data) in mir.basic_blocks().iter_enumerated() {
        for (i, stmt) in bb_data.statements.iter().enumerate() {
            let loc = Location {
                block: bb,
                statement_index: i,
            };
            span_index_items.push((stmt.source_info.span, loc));
        }

        let loc = Location {
            block: bb,
            statement_index: bb_data.statements.len(),
        };
        span_index_items.push((bb_data.terminator().source_info.span, loc));
    }

    SpanIndex::new(span_index_items)
}

pub fn apply_rewrites(tcx: TyCtxt, rewrites: Vec<(Span, Rewrite)>) {
    // TODO: emit new source code properly instead of just printing
    let new_src = apply::apply_rewrites(tcx.sess.source_map(), rewrites);

    for (filename, src) in new_src {
        eprintln!("\n\n ===== BEGIN {:?} =====", filename);
        for line in src.lines() {
            // Omit filecheck directives from the debug output, as filecheck can get confused due
            // to directives matching themselves (e.g. `// CHECK: foo` will match the `foo` in the
            // line `// CHECK: foo`).
            if let Some((pre, _post)) = line.split_once("// CHECK") {
                eprintln!("{}// (FileCheck directive omitted)", pre);
            } else {
                eprintln!("{}", line);
            }
        }
        eprintln!(" ===== END {:?} =====", filename);

        let old_path = match filename {
            FileName::Real(ref rfn) => rfn.local_path().unwrap(),
            _ => {
                eprintln!("can't rewrite non-real file {:?}", filename);
                continue;
            },
        };
        let new_path = old_path.with_extension("new.rs");
        fs::write(new_path, src).unwrap();
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn identity() -> Box<Rewrite> {
        Box::new(Rewrite::Identity)
    }

    fn ref_(rw: Box<Rewrite>) -> Box<Rewrite> {
        Box::new(Rewrite::Ref(rw, Mutability::Not))
    }

    fn index(arr: Box<Rewrite>, idx: Box<Rewrite>) -> Box<Rewrite> {
        Box::new(Rewrite::Index(arr, idx))
    }

    fn cast_usize(rw: Box<Rewrite>) -> Box<Rewrite> {
        Box::new(Rewrite::Cast(rw, "usize".to_owned()))
    }

    /// Test precedence handling in `Rewrite::pretty`
    #[test]
    fn rewrite_pretty_precedence() {
        // Ref vs Index
        assert_eq!(ref_(index(identity(), identity())).to_string(), "&$e[$e]",);

        assert_eq!(
            index(ref_(identity()), ref_(identity())).to_string(),
            "(&$e)[&$e]",
        );

        // Ref vs Cast
        assert_eq!(cast_usize(ref_(identity())).to_string(), "&$e as usize",);

        assert_eq!(ref_(cast_usize(identity())).to_string(), "&($e as usize)",);

        // Cast vs Index
        assert_eq!(
            cast_usize(index(identity(), identity())).to_string(),
            "$e[$e] as usize",
        );

        assert_eq!(
            index(cast_usize(identity()), cast_usize(identity())).to_string(),
            "($e as usize)[$e as usize]",
        );

        // Index vs Index
        assert_eq!(
            index(index(identity(), identity()), identity()).to_string(),
            "$e[$e][$e]",
        );
    }
}
