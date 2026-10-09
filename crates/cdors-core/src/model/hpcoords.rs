//! HEALPix cell centres computed exactly as cdo computes them.
//!
//! cdo (`grid_healpix.cc:hp_generate_kernel`, `hp_index_to_lonlat`) takes the centres from the
//! astrometry.net HEALPix code it bundles (`libhealpix/healpix.c:hp_to_xyz`, then
//! `starutil.inc:xyz2radec`) with offsets dx = dy = 0.5 inside the cell. A different formula
//! (e.g. cdshealpix's) gives the same centres up to the last bits, and that is enough to keep or
//! drop a cell whose centre lies exactly on a `sellonlatbox` edge differently from cdo. The
//! code below is a line-by-line transcription; keep the operation order.

use std::f64::consts::PI;

/// Converts radians to degrees the way cdo does (`RAD2DEG = 180.0 / pi`, one multiplication).
#[inline]
pub fn rad2deg(x: f64) -> f64 {
    x * (180.0 / PI)
}

/// Centre (longitude in [0, 2π), latitude; radians) of the nested-order cell `nested`.
pub fn center_nested(nside: u64, nested: u64) -> (f64, f64) {
    let ns2 = nside * nside;
    let bighp = (nested / ns2) as i64;
    let mut index = nested % ns2;
    // healpixl_nested_to_xy: x takes the even bits, y the odd bits
    let (mut x, mut y) = (0u64, 0u64);
    let mut i = 0;
    while index != 0 {
        x |= (index & 1) << i;
        index >>= 1;
        y |= (index & 1) << i;
        index >>= 1;
        i += 1;
    }
    hp_to_radec(bighp, x as f64 + 0.5, y as f64 + 0.5, nside as f64)
}

/// `hp_to_xyz` followed by `xyzarr2radec`; `x`, `y` already include dx, dy.
fn hp_to_radec(bighp: i64, mut x: f64, mut y: f64, n: f64) -> (f64, f64) {
    let pi = PI;
    let mut chp = bighp;
    let mut equatorial = true;
    let mut zfactor = 1.0;
    if chp <= 3 && (x + y) > n {
        equatorial = false;
        zfactor = 1.0;
    }
    if chp >= 8 && (x + y) < n {
        equatorial = false;
        zfactor = -1.0;
    }
    let (z, phi, rad);
    if equatorial {
        let (mut zoff, mut phioff) = (0.0, 0.0);
        x /= n;
        y /= n;
        if chp <= 3 {
            phioff = 1.0;
        } else if chp <= 7 {
            zoff = -1.0;
            chp -= 4;
        } else {
            phioff = 1.0;
            zoff = -2.0;
            chp -= 8;
        }
        z = 2.0 / 3.0 * (x + y + zoff);
        phi = pi / 4.0 * (x - y + phioff + (2 * chp) as f64);
        rad = (1.0 - z * z).sqrt();
    } else {
        if zfactor == -1.0 {
            std::mem::swap(&mut x, &mut y);
            x = n - x;
            y = n - y;
        }
        let phi_t = if y == n && x == n {
            0.0
        } else {
            pi * (n - y) / (2.0 * ((n - x) + (n - y)))
        };
        let vv = if phi_t < pi / 4.0 {
            (pi * (n - x) / ((2.0 * phi_t - pi) * n) / 3f64.sqrt()).abs()
        } else {
            (pi * (n - y) / (2.0 * phi_t * n) / 3f64.sqrt()).abs()
        };
        let zz = (1.0 - vv) * (1.0 + vv);
        rad = (1.0 + zz).sqrt() * vv;
        z = zz * zfactor;
        phi = if chp >= 8 {
            pi / 2.0 * (chp - 8) as f64 + phi_t
        } else {
            pi / 2.0 * chp as f64 + phi_t
        };
    }
    let phi = if phi < 0.0 { phi + 2.0 * pi } else { phi };
    let (rx, ry, rz) = (rad * phi.cos(), rad * phi.sin(), z);
    // xyz2radec
    let mut ra = ry.atan2(rx);
    if ra < 0.0 {
        ra += 2.0 * PI;
    }
    let dec = if rz.abs() > 0.9 {
        PI / 2.0 - rx.hypot(ry).atan2(rz)
    } else {
        rz.asin()
    };
    (ra, dec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agrees_with_cdshealpix_to_rounding() {
        for depth in [0u8, 1, 3, 6] {
            let nside = 1u64 << depth;
            let layer = cdshealpix::nested::get(depth);
            for h in 0..12 * nside * nside {
                let (lon, lat) = center_nested(nside, h);
                let (l2, b2) = layer.center(h);
                assert!((lon - l2).abs() < 1e-12, "lon {h} {lon} {l2}");
                assert!((lat - b2).abs() < 1e-12, "lat {h} {lat} {b2}");
            }
        }
    }
}
