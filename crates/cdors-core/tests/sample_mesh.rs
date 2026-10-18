//! Sampling weights (`cdors_mesh::sample`) on the FESOM2 pi mesh (`$CDORS_FIXTURES/fesom_pi`,
//! else the pool copy; skipped without either) and on the one-place view of HEALPix hpz2.

use cdors_core::io::ChunkSource;
use cdors_core::model::mesh_cache;
use cdors_core::remap::target::from_name;
use cdors_mesh::geom::unit_vector;
use cdors_mesh::sample::{self, Method, Sampler};
use cdors_mesh::{Csr, Mesh, Place};
use std::path::PathBuf;
use std::sync::Arc;

fn pi_mesh() -> Option<Arc<Mesh>> {
    let dir = std::env::var_os("CDORS_FIXTURES")
        .map(|d| PathBuf::from(d).join("fesom_pi"))
        .filter(|d| d.is_dir())
        .or_else(|| {
            Some(PathBuf::from("/pool/data/AWICM/FESOM2/MESHES_FESOM2.1/pi")).filter(|d| d.is_dir())
        })?;
    let spec = mesh_cache::parse_spec(&format!("fesom:{}", dir.display()))?.unwrap();
    Some(mesh_cache::load(&spec).unwrap().mesh)
}

/// 2000 points spread over the sphere (a small LCG) and a Drake Passage path.
fn points() -> (Vec<f64>, Vec<f64>) {
    let mut s = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (s >> 11) as f64 / (1u64 << 53) as f64
    };
    let (mut lon, mut lat): (Vec<f64>, Vec<f64>) = (0..2000)
        .map(|_| {
            (
                360.0 * rnd() - 180.0,
                (2.0 * rnd() - 1.0).asin().to_degrees(),
            )
        })
        .unzip();
    let p = sample::sample_path(&[(-65.0, -55.0), (-60.0, -65.0)], 20.0).unwrap();
    lon.extend(p.lon);
    lat.extend(p.lat);
    (lon, lat)
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

/// Rows of linear weights: sum 1, positive, and a field linear in x, y, z reproduced exactly at
/// the point's central projection onto the plane of the row's cells.
fn check_linear(place: &Place, w: &Csr, lon: &[f64], lat: &[f64]) {
    let f = |p: &[f64; 3]| 2.0 + 0.3 * p[0] - 1.1 * p[1] + 0.7 * p[2];
    for r in 0..w.rows() {
        let (c, wr) = (w.row(r), w.weights(r).unwrap());
        if c.is_empty() {
            continue;
        }
        assert!(c.len() <= 3 && wr.iter().all(|&x| x > 0.0), "row {r}");
        assert!((wr.iter().sum::<f64>() - 1.0).abs() < 1e-12, "row {r}");
        let p: Vec<[f64; 3]> = c
            .iter()
            .map(|&i| unit_vector(place.lon[i as usize], place.lat[i as usize]))
            .collect();
        let mut proj = [0.0; 3];
        for (k, pk) in p.iter().enumerate() {
            (0..3).for_each(|d| proj[d] += wr[k] * pk[d]);
        }
        let x = cross(&proj, &unit_vector(lon[r], lat[r]));
        assert!(
            dot(&x, &x).sqrt() < 1e-12,
            "row {r}: off the ray of the point"
        );
        let got: f64 = p.iter().zip(wr).map(|(pk, wk)| wk * f(pk)).sum();
        assert!((got - f(&proj)).abs() < 1e-12, "row {r}");
    }
}

#[test]
fn pi_mesh_linear_nn_levels_and_step() {
    let Some(mesh) = pi_mesh() else {
        eprintln!("skipped: no FESOM2 pi mesh (CDORS_FIXTURES/fesom_pi or /pool)");
        return;
    };
    let nodes = mesh.place("nodes").unwrap();
    let tri = sample::triangles(&mesh, "nodes")
        .unwrap()
        .expect("elem_nodes");
    let s = Sampler::new(nodes, Some(tri.clone())).unwrap();
    let (lon, lat) = points();
    let lin = s.weights(Method::Linear, &lon, &lat);
    check_linear(nodes, &lin.weights, &lon, &lat);
    // brute force over all elements: a row lies in a containing element; an empty row has none
    let v: Vec<[f64; 3]> = (0..nodes.size)
        .map(|i| unit_vector(nodes.lon[i], nodes.lat[i]))
        .collect();
    let contains = |e: usize, q: &[f64; 3], tol: f64| {
        let c = tri.row(e);
        let (a, b, cc) = (&v[c[0] as usize], &v[c[1] as usize], &v[c[2] as usize]);
        let det = dot(a, &cross(b, cc));
        let m = [
            dot(q, &cross(b, cc)),
            dot(q, &cross(cc, a)),
            dot(q, &cross(a, b)),
        ];
        let s = m[0] + m[1] + m[2];
        s * det > 0.0 && m.iter().all(|&x| x / s >= tol)
    };
    for r in 0..lon.len() {
        let q = unit_vector(lon[r], lat[r]);
        let cols = lin.weights.row(r);
        if cols.is_empty() {
            assert!(
                (0..tri.rows()).all(|e| !contains(e, &q, 1e-9)),
                "row {r} ({}, {}): outside, but an element contains it",
                lon[r],
                lat[r]
            );
        } else {
            assert!(
                (0..tri.rows())
                    .any(|e| contains(e, &q, -1e-12) && cols.iter().all(|c| tri.row(e).contains(c))),
                "row {r}: no containing element has these nodes"
            );
        }
    }
    assert!(
        lin.outside > 0 && lin.outside < lon.len() / 2,
        "{}",
        lin.outside
    );
    // nn: brute force, smallest index on ties
    let nn = s.weights(Method::Nn, &lon, &lat);
    for r in 0..lon.len() {
        let q = unit_vector(lon[r], lat[r]);
        let d = |i: usize| {
            let x = [v[i][0] - q[0], v[i][1] - q[1], v[i][2] - q[2]];
            dot(&x, &x)
        };
        let want = (0..nodes.size)
            .min_by(|&a, &b| d(a).total_cmp(&d(b)).then(a.cmp(&b)))
            .unwrap();
        assert_eq!(nn.weights.row(r), &[want as u32], "row {r}");
    }
    // levels: nn = the node's own; linear = the shallowest of the row's nodes
    let vert = mesh.vertical.as_ref().unwrap();
    let wet = vert.wet("nodes").unwrap();
    for (w, m) in [(&nn.weights, "nn"), (&lin.weights, "linear")] {
        let pw = sample::wet_levels(w, wet);
        let depth = sample::bottom_depth(&pw, &vert.interfaces);
        for r in 0..lon.len() {
            let cells = w.row(r);
            if cells.is_empty() {
                assert!(pw.range(r).is_empty() && depth[r].is_nan());
                continue;
            }
            let b = cells.iter().map(|&c| wet.bottom[c as usize]).min().unwrap();
            assert_eq!(pw.range(r).end, b as usize, "{m} row {r}");
            assert_eq!(depth[r], vert.interfaces[b as usize].abs(), "{m} row {r}");
        }
    }
    // the default step across Drake Passage: the node spacing of pi there (~2 degrees; pi has
    // 3140 nodes, ~400 km apart on average)
    let st = s
        .default_step_km(&[(-65.0, -55.0), (-60.0, -65.0)])
        .unwrap();
    assert!((100.0..400.0).contains(&st), "{st}");
    // elements: local Delaunay of the element centres
    let elems = mesh.place("elements").unwrap();
    let se = Sampler::new(elems, None).unwrap();
    let le = se.weights(Method::Linear, &lon, &lat);
    check_linear(elems, &le.weights, &lon, &lat);
}

#[test]
fn healpix_view_step_and_linear() {
    let src = from_name("hpz2").unwrap();
    let ds = src.dataset();
    let var = ds.data_vars().next().unwrap();
    let grid = &ds.grids[var.grid.unwrap()];
    let mesh = grid.mesh_view(ds, var, &|v| src.read_var(v)).unwrap();
    let cells = mesh.place("cells").unwrap();
    let s = Sampler::new(cells, None).unwrap();
    // HEALPix spacing: about the square root of the cell area, 4 pi / 192
    let h = (4.0 * std::f64::consts::PI / 192.0).sqrt() * sample::EARTH_RADIUS_KM;
    for wp in [[(-60.0, 0.0), (60.0, 0.0)], [(0.0, 60.0), (180.0, 60.0)]] {
        let st = s.default_step_km(&wp).unwrap();
        assert!((0.7 * h..1.2 * h).contains(&st), "{st} vs {h}");
    }
    let (lon, lat) = points();
    let lin = s.weights(Method::Linear, &lon, &lat);
    assert_eq!(lin.outside, 0);
    check_linear(cells, &lin.weights, &lon, &lat);
}
