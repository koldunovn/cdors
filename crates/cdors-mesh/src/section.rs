//! The faces a section crosses: where a great-circle path enters and leaves each triangle of a
//! mesh, for fluxes across the path (`transport`).
//!
//! The path's arcs are intersected with the triangles' edges (the great-circle arcs between
//! their nodes). Each triangle the path passes through gets the broken line from the midpoint of
//! the edge where the path enters it, through its centre, to the midpoint of the edge where it
//! leaves: the faces of the median-dual cells across which FESOM2 computes its fluxes (also
//! pyfesom2's `cross_section_transport`). With one velocity per triangle the flux across that
//! line depends only on its two ends, so each triangle keeps the chord between the two
//! midpoints, as its east and north components at the triangle's centre.
//!
//! **Sides.** A node lies left of an arc a → b if (a × b) · p ≥ 0, so a path through a node
//! crosses the triangles around it consistently (a great circle crosses 0 or 2 edges of each).
//! A triangle where the path starts or ends (an end in the ocean) keeps one crossing without a
//! partner; it is left out and counted ([`Section::open_ends`]).

use crate::geom::unit_vector;
use crate::{Csr, MeshError, Result};

/// One triangle the path crosses: the chord from the midpoint of its entry edge to that of its
/// exit edge on the unit sphere, east and north at the triangle's centre (several passes
/// summed).
#[derive(Debug, Clone, PartialEq)]
pub struct Crossing {
    pub elem: u32,
    pub east: f64,
    pub north: f64,
}

/// The triangles a path crosses.
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    /// In the order the path first enters them.
    pub crossings: Vec<Crossing>,
    /// Triangles where the path starts or ends, left out.
    pub open_ends: usize,
    /// Length of the path on the unit sphere (radians).
    pub length: f64,
}

fn dot(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: &[f64; 3], b: &[f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn norm(a: &[f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

fn unit(a: [f64; 3]) -> [f64; 3] {
    let n = norm(&a);
    [a[0] / n, a[1] / n, a[2] / n]
}

/// The triangles of `elem_nodes` (rows: triangles, columns: nodes at `node_lon`, `node_lat`,
/// degrees) that the great-circle path through `waypoints` crosses; `centre_lon`, `centre_lat`
/// are the triangles' centres (degrees).
pub fn crossings(
    node_lon: &[f64],
    node_lat: &[f64],
    elem_nodes: &Csr,
    centre_lon: &[f64],
    centre_lat: &[f64],
    waypoints: &[(f64, f64)],
) -> Result<Section> {
    let p: Vec<[f64; 3]> = node_lon
        .iter()
        .zip(node_lat)
        .map(|(&x, &y)| unit_vector(x, y))
        .collect();
    // (triangle, position along the path, midpoint of the crossed edge)
    let mut hits: Vec<(u32, f64, [f64; 3])> = Vec::new();
    let mut length = 0.0;
    for w in waypoints.windows(2) {
        let (a, b) = (unit_vector(w[0].0, w[0].1), unit_vector(w[1].0, w[1].1));
        let n = cross(&a, &b);
        if norm(&n) < 1e-12 {
            if dot(&a, &b) > 0.0 {
                continue; // the same point twice
            }
            return Err(MeshError::Invalid(format!(
                "waypoints ({}, {}) and ({}, {}) are antipodal: no single great circle joins them",
                w[0].0, w[0].1, w[1].0, w[1].1
            )));
        }
        let side: Vec<bool> = p.iter().map(|q| dot(&n, q) >= 0.0).collect();
        for e in 0..elem_nodes.rows() {
            let r = elem_nodes.row(e);
            if r.len() != 3 {
                return Err(MeshError::Invalid(format!(
                    "element {e} has {} nodes, not 3",
                    r.len()
                )));
            }
            let s = [side[r[0] as usize], side[r[1] as usize], side[r[2] as usize]];
            if s[0] == s[1] && s[1] == s[2] {
                continue;
            }
            for (i, j) in [(0, 1), (1, 2), (2, 0)] {
                if s[i] == s[j] {
                    continue;
                }
                let (pi, pj) = (&p[r[i] as usize], &p[r[j] as usize]);
                let mut x = unit(cross(&n, &cross(pi, pj)));
                let m = [pi[0] + pj[0], pi[1] + pj[1], pi[2] + pj[2]];
                if dot(&x, &m) < 0.0 {
                    x = [-x[0], -x[1], -x[2]];
                }
                // on the arc a -> b, not elsewhere on its great circle
                let ax = cross(&a, &x);
                if dot(&ax, &n) < 0.0 || dot(&cross(&x, &b), &n) < 0.0 {
                    continue;
                }
                hits.push((e as u32, length + norm(&ax).atan2(dot(&a, &x)), unit(m)));
            }
        }
        length += norm(&n).atan2(dot(&a, &b));
    }
    hits.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.total_cmp(&y.1)));
    let mut out: Vec<(f64, Crossing)> = Vec::new();
    let mut open_ends = 0;
    let mut i = 0;
    while i < hits.len() {
        let e = hits[i].0;
        let j = i + hits[i..].iter().take_while(|h| h.0 == e).count();
        let group = &hits[i..j];
        open_ends += group.len() % 2;
        if group.len() >= 2 {
            let (lo, la) = (centre_lon[e as usize].to_radians(), centre_lat[e as usize].to_radians());
            let east = [-lo.sin(), lo.cos(), 0.0];
            let north = [-la.sin() * lo.cos(), -la.sin() * lo.sin(), la.cos()];
            let mut c = Crossing { elem: e, east: 0.0, north: 0.0 };
            for [h0, h1] in group.as_chunks::<2>().0 {
                let (m0, m1) = (h0.2, h1.2);
                let d = [m1[0] - m0[0], m1[1] - m0[1], m1[2] - m0[2]];
                c.east += dot(&d, &east);
                c.north += dot(&d, &north);
            }
            out.push((group[0].1, c));
        }
        i = j;
    }
    out.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.elem.cmp(&y.1.elem)));
    Ok(Section {
        crossings: out.into_iter().map(|(_, c)| c).collect(),
        open_ends,
        length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two triangles forming the box 0..2 E, -1..1 N, split along its diagonal.
    fn square() -> (Vec<f64>, Vec<f64>, Csr, Vec<f64>, Vec<f64>) {
        let lon = vec![0.0, 2.0, 2.0, 0.0];
        let lat = vec![-1.0, -1.0, 1.0, 1.0];
        let en = Csr::fixed(3, vec![0, 1, 2, 0, 2, 3], 4).unwrap();
        let (clon, clat) = (vec![4.0 / 3.0, 2.0 / 3.0], vec![-1.0 / 3.0, 1.0 / 3.0]);
        (lon, lat, en, clon, clat)
    }

    #[test]
    fn across_the_box() {
        let (lon, lat, en, clon, clat) = square();
        // west to east along 0.5 N: enters through the west edge, leaves through the east edge
        let s = crossings(&lon, &lat, &en, &clon, &clat, &[(-1.0, 0.5), (3.0, 0.5)]).unwrap();
        assert_eq!(s.open_ends, 0);
        assert_eq!(s.crossings.iter().map(|c| c.elem).collect::<Vec<_>>(), vec![1, 0]);
        // the chords join the midpoints of the west, the diagonal and the east edge: together
        // 2 degrees east, no net north
        let e: f64 = s.crossings.iter().map(|c| c.east).sum();
        let n: f64 = s.crossings.iter().map(|c| c.north).sum();
        assert!((e - 2f64.to_radians()).abs() < 1e-5, "{e}");
        assert!(n.abs() < 1e-5, "{n}");
        assert!((s.length - 4f64.to_radians() * 0.5f64.to_radians().cos()).abs() < 1e-6);
    }

    #[test]
    fn ends_inside_and_joints() {
        let (lon, lat, en, clon, clat) = square();
        // ends inside the box: one crossing of triangle 1 left over
        let s = crossings(&lon, &lat, &en, &clon, &clat, &[(-1.0, 0.5), (0.5, 0.5)]).unwrap();
        assert_eq!((s.crossings.len(), s.open_ends), (0, 1));
        // a joint inside triangle 0: the two arcs make one pass through it
        let s = crossings(
            &lon,
            &lat,
            &en,
            &clon,
            &clat,
            &[(1.0, -2.0), (1.6, -0.5), (3.0, -0.5)],
        )
        .unwrap();
        assert_eq!(s.open_ends, 0);
        assert_eq!(s.crossings.iter().map(|c| c.elem).collect::<Vec<_>>(), vec![0]);
        // from the south edge's midpoint (1, -1) to the east edge's (2, 0)
        assert!((s.crossings[0].east - 1f64.to_radians()).abs() < 1e-3);
        assert!((s.crossings[0].north - 1f64.to_radians()).abs() < 1e-3);
        assert!(crossings(&lon, &lat, &en, &clon, &clat, &[(0.0, 0.0), (180.0, 0.0)]).is_err());
    }
}
