//! gribscan references to a reduced Gaussian GRIB file (fixture `grb_rgg` of
//! tests/make_fixtures.sh), where cdo is no reference: cdo 2.6 weights reduced Gaussian grids
//! equally ("Grid cell bounds not available"), cdors by the cell areas of the rows.

use cdors_core::model::area::{self, AreaSource};

#[test]
fn reduced_gaussian_grid_is_area_weighted() {
    let Ok(fx) = std::env::var("CDORS_FIXTURES") else {
        eprintln!("skipped: CDORS_FIXTURES not set");
        return;
    };
    let path = format!("{fx}/grb_rgg.json");
    if !std::path::Path::new(&path).exists() {
        eprintln!("skipped: {path} missing (run tests/make_fixtures.sh)");
        return;
    }
    let src = cdors_core::io::open(&path).expect("open");
    let ds = src.dataset();
    let var = ds.var("2t").expect("2t");
    let g = &ds.grids[var.grid.expect("grid")];
    assert_eq!(g.reduced.as_ref().map(|r| r.counts.len()), Some(64));
    let w = area::cell_weights(ds, var, g, &|n| src.read_var(n)).expect("weights");
    assert_eq!(w.source, AreaSource::ReducedRows);
    let x = src
        .read_chunk("2t", &[0, 0])
        .unwrap()
        .decode()
        .unwrap()
        .values
        .to_f64();
    // 250 + 40 cos²(lat) + 5 sin(lon) cos(lat): 276.667 over the sphere (equal weights: 275.07)
    let mean: f64 = x.iter().zip(&w.values).map(|(x, w)| x * w).sum();
    assert!((mean - 276.6667).abs() < 0.01, "{mean}");
}
