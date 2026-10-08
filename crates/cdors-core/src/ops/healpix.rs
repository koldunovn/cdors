//! `hpdegrade` and `hpupgrade`: change the resolution of a HEALPix grid (cdo 2.6,
//! `src/operators/Healpix.cc`).
//!
//! Arguments are cdo's `key=value` parameters: `nside=<n>`, `zoom=<z>` (nside = 2^z) or
//! `fact=<f>` (mutually exclusive), `order=nested|ring` (output order, default the input's),
//! `stat=mean|avg` (degrade only) and `power=<p>` (values are scaled by (nside_in/nside_out)^-p).
//!
//! Degrading averages the `fact²` nested children of every output cell, summing in double
//! precision. With missing values in a field, `stat=mean` (default) averages the valid children
//! and is missing only when none is valid; `stat=avg` is missing when any child is missing.
//! Upgrading copies every cell to its children. Ring-ordered input is converted to nested order
//! first and the output to the requested order last, as cdo does.
//!
//! The output grid is written like cdo writes it for a grid-mapping HEALPix input: the input's
//! grid-mapping variable with the new `healpix_nside`/`healpix_order`, cell dimension `cells`.
//! Access class: whole extent over space (complete fields, parallel over timesteps and levels),
//! through the same field kernel as remapping.

use super::remap::{FieldKernel, FieldMap, FieldVar, no_pending_fold, trailing_hdims};
use crate::chain::OpNode;
use crate::error::{Error, ErrorCode, Result};
use crate::io::Values;
use crate::model::{AttrValue, DType, GridKind, HealpixOrder, VarDim};
use crate::plan::{Desc, Fold, IndexMap};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stat {
    Mean,
    Avg,
}

struct HpMap {
    degrade: bool,
    fact: usize,
    stat: Stat,
    scale: f64,
    nside_in: u64,
    nside_out: u64,
    /// `ring_to_nested[r]` = nested index of ring cell r (input grid, when ring ordered).
    in_ring: Option<Vec<usize>>,
    /// `nested_to_ring[n]` = ring index of nested cell n (output grid, when ring ordered).
    out_ring: Option<Vec<usize>>,
}

/// Ring index -> nested index for every cell of a grid with `nside`.
fn ring_to_nested(nside: u64) -> Vec<usize> {
    let layer = cdshealpix::nested::get(nside.trailing_zeros() as u8);
    (0..12 * nside * nside)
        .map(|r| layer.from_ring(r) as usize)
        .collect()
}

impl HpMap {
    /// One field (nested order in and out, values as f64, NaN missing); `round` rounds to the
    /// field's type.
    fn field(&self, v1: &[f64], v2: &mut [f64], round: fn(f64) -> f64) {
        let nv = self.fact * self.fact;
        if self.degrade {
            let has_missing = v1.iter().any(|x| x.is_nan());
            for (i, o) in v2.iter_mut().enumerate() {
                let ch = &v1[i * nv..(i + 1) * nv];
                *o = if has_missing {
                    let (mut sum, mut n) = (0.0f64, 0usize);
                    for &x in ch {
                        if !x.is_nan() {
                            sum += x;
                            n += 1;
                        }
                    }
                    let ok = match self.stat {
                        Stat::Mean => n > 0,
                        Stat::Avg => n == nv,
                    };
                    if ok {
                        round((sum / n as f64) * self.scale)
                    } else {
                        f64::NAN
                    }
                } else {
                    let sum: f64 = ch.iter().sum();
                    // cdo: stat_mean returns the field type, then the scale is applied
                    round(round(sum / nv as f64) * self.scale)
                };
            }
        } else {
            for (i, &x) in v1.iter().enumerate() {
                let y = if x.is_nan() {
                    f64::NAN
                } else {
                    round(x * self.scale)
                };
                v2[i * nv..(i + 1) * nv].fill(y);
            }
        }
    }
}

impl FieldMap for HpMap {
    fn apply(&self, _var: usize, src: &Values, n: usize, out: DType) -> Result<Values> {
        use rayon::prelude::*;
        let n1 = (12 * self.nside_in * self.nside_in) as usize;
        let n2 = (12 * self.nside_out * self.nside_out) as usize;
        let src = src.to_f64();
        if src.len() != n * n1 {
            return Err(Error::internal("HEALPix field size"));
        }
        let round: fn(f64) -> f64 = if out == DType::F32 {
            |x| x as f32 as f64
        } else {
            |x| x
        };
        let mut dst = vec![0.0f64; n * n2];
        src.par_chunks(n1)
            .zip(dst.par_chunks_mut(n2))
            .for_each(|(s, d)| {
                let nested;
                let s = match &self.in_ring {
                    Some(r2n) => {
                        let mut v = vec![0.0; n1];
                        for (r, &x) in s.iter().enumerate() {
                            v[r2n[r]] = x;
                        }
                        nested = v;
                        &nested[..]
                    }
                    None => s,
                };
                match &self.out_ring {
                    Some(n2r) => {
                        let mut v = vec![0.0; n2];
                        self.field(s, &mut v, round);
                        for (k, &x) in v.iter().enumerate() {
                            d[n2r[k]] = x;
                        }
                    }
                    None => self.field(s, d, round),
                }
            });
        Ok(match out {
            DType::F32 => Values::F32(dst.into_iter().map(|x| x as f32).collect()),
            _ => Values::F64(dst),
        })
    }
}

fn bad(op: &str, msg: impl Into<String>) -> Error {
    Error::bad_arguments(msg.into())
        .with("operator", op.to_owned())
        .with_hint(format!(
            "usage: {op},nside=<n>|zoom=<z>|fact=<f>[,order=nested|ring][,stat=mean|avg][,power=<p>]"
        ))
}

/// Output description of `hpdegrade` / `hpupgrade`.
pub fn describe(node: &OpNode, mut inputs: Vec<Desc>) -> Result<Desc> {
    let op = node.name.as_str();
    let degrade = op == "hpdegrade";
    let input = inputs
        .pop()
        .ok_or_else(|| Error::internal("hpdegrade without input"))?;
    no_pending_fold(op, &input)?;

    // parameters
    let (mut nside_out, mut zoom, mut fact) = (0u64, None::<u32>, 1u64);
    let (mut order_out, mut stat, mut power) = (None, Stat::Mean, 0.0f64);
    for a in &node.args {
        let (k, v) = a
            .split_once('=')
            .ok_or_else(|| bad(op, format!("parameter '{a}' is not key=value")))?;
        let int = || {
            v.trim()
                .parse::<u64>()
                .map_err(|_| bad(op, format!("{k}={v}: not a positive integer")))
        };
        match k.trim() {
            "nside" => nside_out = int()?,
            "zoom" => zoom = Some(int()? as u32),
            "fact" => fact = int()?,
            "order" => {
                order_out = Some(match v.trim().to_ascii_lowercase().as_str() {
                    "nested" | "nest" => HealpixOrder::Nested,
                    "ring" => HealpixOrder::Ring,
                    _ => return Err(bad(op, format!("order={v}: nested or ring"))),
                })
            }
            "stat" if degrade => {
                stat = match v.trim() {
                    "mean" => Stat::Mean,
                    "avg" => Stat::Avg,
                    _ => return Err(bad(op, format!("stat={v} unsupported (mean or avg)"))),
                }
            }
            "power" => {
                power = v
                    .trim()
                    .parse()
                    .map_err(|_| bad(op, format!("power={v}: not a number")))?
            }
            _ => return Err(bad(op, format!("unknown parameter '{k}'"))),
        }
    }
    if fact > 1 && nside_out > 0 {
        return Err(bad(op, "parameter 'fact' can't be combined with 'nside'"));
    }
    if fact > 1 && zoom.is_some() {
        return Err(bad(op, "parameter 'fact' can't be combined with 'zoom'"));
    }
    if zoom.is_some() && nside_out > 0 {
        return Err(bad(op, "parameter 'zoom' can't be combined with 'nside'"));
    }
    if let Some(z) = zoom {
        nside_out = 1u64 << z;
    }

    // the input grid: one complete HEALPix grid
    let mut gis: Vec<usize> = input.vars.iter().filter_map(|v| v.grid).collect();
    gis.sort_unstable();
    gis.dedup();
    if gis.len() != 1 {
        return Err(Error::new(
            ErrorCode::UnsupportedGrid,
            format!(
                "{op}: needs exactly one horizontal grid, found {}",
                gis.len()
            ),
        )
        .with("operator", op.to_owned())
        .with_hint("select variables on the HEALPix grid with -selname"));
    }
    let g = &input.grids[gis[0]];
    if g.kind != GridKind::Healpix || !g.is_healpix() {
        return Err(Error::new(
            ErrorCode::UnsupportedGrid,
            format!("{op}: input grid is not a complete HEALPix grid"),
        )
        .with("operator", op.to_owned()));
    }
    let hp = g.base.healpix.clone().expect("healpix grid");
    let nside_in = hp.nside;
    if !nside_in.is_power_of_two() {
        return Err(bad(op, "input HEALPix nside must be a power of two"));
    }
    if nside_out == 0 {
        nside_out = match (fact > 1, degrade) {
            (true, true) => nside_in / fact,
            (true, false) => nside_in * fact,
            _ => nside_in,
        };
    } else if degrade {
        if nside_out > nside_in {
            return Err(bad(
                op,
                format!("nside={nside_out} must be less than input nside={nside_in}"),
            ));
        }
        fact = nside_in / nside_out;
    } else {
        if nside_out < nside_in {
            return Err(bad(
                op,
                format!("nside={nside_out} must be greater than input nside={nside_in}"),
            ));
        }
        fact = nside_out / nside_in;
    }
    if nside_out == 0 || !nside_out.is_power_of_two() {
        return Err(bad(op, "parameter nside must be a power of two"));
    }
    let order_in = hp.order;
    let order_out = order_out.unwrap_or(order_in);
    let scale = if power.abs() > 0.0 {
        (nside_in as f64 / nside_out as f64).powf(-power)
    } else {
        1.0
    };
    let n2 = (12 * nside_out * nside_out) as usize;

    // output grid: the input's grid mapping with the new nside and order, dimension `cells`
    let mut og = g.clone();
    og.base.size = n2;
    og.base.xsize = n2;
    og.base.ysize = 0;
    og.base.dims = vec!["cells".into()];
    if let Some(h) = og.base.healpix.as_mut() {
        h.nside = nside_out;
        h.order = order_out;
        h.index_var = None;
    }
    if let Some(m) = og.base.mapping.as_mut() {
        let order_s = match order_out {
            HealpixOrder::Nested => "nested",
            HealpixOrder::Ring => "ring",
        };
        for (k, v) in m.attrs.0.iter_mut() {
            match k.as_str() {
                "healpix_nside" => *v = AttrValue::Ints(vec![nside_out as i64]),
                "refinement_level" => {
                    *v = AttrValue::Ints(vec![i64::from(nside_out.trailing_zeros())])
                }
                "healpix_order" | "indexing_scheme" => *v = AttrValue::Text(order_s.into()),
                _ => {}
            }
        }
    }
    og.sel = vec![IndexMap::identity(n2)];
    og.xvals = None;

    let mut fvars = Vec::new();
    let mut out_vars = Vec::new();
    for v in &input.vars {
        if v.grid.is_none() {
            return Err(Error::new(
                ErrorCode::NoCoordinates,
                format!("variable '{}' has no horizontal grid", v.name),
            )
            .with("operator", op.to_owned()));
        }
        let hfirst = trailing_hdims(op, v)?;
        fvars.push(FieldVar {
            hfirst,
            src_size: g.size(),
            out_hdims: vec![n2],
            out_dtype: v.dtype,
        });
        let mut ov = v.clone();
        ov.dims.truncate(hfirst);
        ov.dims.push(VarDim {
            name: "cells".into(),
            size: n2,
            role: crate::model::DimRole::Horizontal,
        });
        ov.grid = Some(0);
        out_vars.push(ov);
    }
    let map = HpMap {
        degrade,
        fact: fact as usize,
        stat,
        scale,
        nside_in,
        nside_out,
        in_ring: (order_in == HealpixOrder::Ring).then(|| ring_to_nested(nside_in)),
        out_ring: (order_out == HealpixOrder::Ring).then(|| {
            // nested -> ring is the inverse permutation of ring -> nested
            let r2n = ring_to_nested(nside_out);
            let mut n2r = vec![0usize; r2n.len()];
            for (r, &n) in r2n.iter().enumerate() {
                n2r[n] = r;
            }
            n2r
        }),
    };
    Ok(Desc {
        attrs: input.attrs.clone(),
        vars: out_vars,
        grids: vec![og],
        zaxes: input.zaxes.clone(),
        time: input.time.clone(),
        fold: Some(Fold {
            input: Box::new(input),
            kernel: Arc::new(FieldKernel {
                map: Arc::new(map),
                vars: fvars,
            }),
        }),
    })
}
