//! A [`Mesh`] as UGRID 1.0 arrays and back: the layout of the mesh cache (a Zarr store written
//! by cdors-core), readable by UGRID tools (xugrid, uxarray) for its standard parts.
//!
//! The crate stays free of netCDF and Zarr: [`write`] hands named arrays and attributes to a
//! [`UgridSink`], and [`read`] rebuilds the mesh from any [`ArrayReader`] over what was written,
//! with the per-level areas, polygons and tables lazy again.
//!
//! **Layout.** Dimensions are named after the places. For a place `p`:
//! - `p_lon(p)`, `p_lat(p)` (degrees), `p_area(p)`;
//! - `p_area_per_level(p_nlev, p)`, one chunk per level, if the place has per-level areas;
//! - `p_poly_offsets(p_poly_n)`, `p_poly_lon(p_poly_nv)`, `p_poly_lat(p_poly_nv)` for polygons;
//! - `p_wet_top(p)`, `p_wet_bottom(p)` (0-based, half open) if the vertical has its wet levels.
//!
//! A table `t` is `t(rows_place, t_width)`, padded with −1 (`_FillValue`), `start_index` 0,
//! with `cdors_rows`/`cdors_cols`. The vertical is `cdors_interfaces(cdors_nz)` and
//! `cdors_mid(cdors_nz1)`. The 0-d variable `mesh` is the UGRID topology (`cf_role =
//! mesh_topology`, `node_coordinates` for the place `nodes`, `face_node_connectivity` and
//! `face_coordinates` for a three-column table into `nodes`), and lists `cdors_places` and
//! `cdors_tables`. Name, model, files, units decision and notes are global `cdors_*`
//! attributes.

use crate::mesh::csr_bytes;
use crate::{
    ArrayReader, Attr, Csr, Lazy, Levels, Mesh, MeshError, Place, Polygons, Result, Source, Table,
    Thickness, Vertical, WetLevels,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Version of the layout (global attribute `cdors_mesh_format`).
pub const FORMAT: u32 = 1;

/// Storage type of an array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    F64,
    I32,
}

/// An array to create: dimensions (name, size), chunk shape, type, attributes.
#[derive(Debug, Clone)]
pub struct ArraySpec {
    pub name: String,
    pub dims: Vec<(String, usize)>,
    pub chunks: Vec<usize>,
    pub kind: Kind,
    pub attrs: Vec<(String, Attr)>,
}

/// Where [`write`] puts the arrays (cdors-core: a Zarr store).
pub trait UgridSink {
    /// Global attributes.
    fn global(&mut self, attrs: &[(String, Attr)]) -> Result<()>;
    /// Creates an array.
    fn create(&mut self, spec: &ArraySpec) -> Result<()>;
    /// Writes the values of the chunk that starts at `first` along the first dimension (whole
    /// chunks of the spec, C order; all of a 0-d or 1-D array written in one chunk).
    fn write(&mut self, name: &str, first: usize, values: &[f64]) -> Result<()>;
}

fn text(k: &str, v: impl Into<String>) -> (String, Attr) {
    (k.to_owned(), Attr::Text(v.into()))
}

fn num(k: &str, v: f64) -> (String, Attr) {
    (k.to_owned(), Attr::Num(vec![v]))
}

fn one_d(name: &str, dim: &str, n: usize, kind: Kind, attrs: Vec<(String, Attr)>) -> ArraySpec {
    ArraySpec {
        name: name.to_owned(),
        dims: vec![(dim.to_owned(), n)],
        chunks: vec![n.max(1)],
        kind,
        attrs,
    }
}

fn put(sink: &mut dyn UgridSink, spec: ArraySpec, values: &[f64]) -> Result<()> {
    sink.create(&spec)?;
    sink.write(&spec.name, 0, values)
}

/// Writes `mesh` (plus the global attributes `extra`) into `sink`. Lazy components are read
/// once each without being kept ([`Lazy::get_uncached`]): per-level areas level by level, so
/// that writing a large mesh holds one level at a time.
pub fn write(mesh: &Mesh, extra: &[(String, Attr)], sink: &mut dyn UgridSink) -> Result<()> {
    let s = &mesh.source;
    let mut global = vec![
        text("Conventions", "UGRID-1.0"),
        num("cdors_mesh_format", FORMAT as f64),
        text("cdors_name", mesh.name.clone()),
        text("cdors_model", s.model.clone()),
        text(
            "cdors_files",
            s.files
                .iter()
                .map(|f| f.display().to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        text("cdors_notes", s.notes.join("\n")),
    ];
    if let Some(u) = &s.units {
        global.push(text("cdors_units", u.clone()));
    }
    global.extend(extra.iter().cloned());
    sink.global(&global)?;

    // the UGRID topology variable
    let mut topo = vec![
        text("cf_role", "mesh_topology"),
        num("topology_dimension", 2.0),
        text("long_name", format!("mesh {}", mesh.name)),
        text(
            "cdors_places",
            mesh.places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        text(
            "cdors_tables",
            mesh.topology
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
    ];
    if mesh.place("nodes").is_some() {
        topo.push(text("node_coordinates", "nodes_lon nodes_lat"));
    }
    let faces = mesh
        .topology
        .iter()
        .find(|t| t.cols_place == "nodes" && t.name.ends_with("_nodes"));
    if let Some(t) = faces {
        topo.push(text("face_node_connectivity", t.name.clone()));
        topo.push(text("face_dimension", t.rows_place.clone()));
        topo.push(text(
            "face_coordinates",
            format!("{0}_lon {0}_lat", t.rows_place),
        ));
    }
    put(
        sink,
        ArraySpec {
            name: "mesh".into(),
            dims: Vec::new(),
            chunks: Vec::new(),
            kind: Kind::I32,
            attrs: topo,
        },
        &[0.0],
    )?;

    for p in &mesh.places {
        write_place(p, faces.map(|t| t.rows_place.as_str()), sink)?;
    }
    for t in &mesh.topology {
        write_table(mesh, t, sink)?;
    }
    if let Some(v) = &mesh.vertical {
        let thick = match v.thickness {
            Thickness::Static => "static",
        };
        put(
            sink,
            one_d(
                "cdors_interfaces",
                "cdors_nz",
                v.interfaces.len(),
                Kind::F64,
                vec![
                    text("long_name", "level interfaces"),
                    text("cdors_thickness", thick),
                ],
            ),
            &v.interfaces,
        )?;
        put(
            sink,
            one_d(
                "cdors_mid",
                "cdors_nz1",
                v.mid.len(),
                Kind::F64,
                vec![text("long_name", "mid-levels")],
            ),
            &v.mid,
        )?;
        for w in &v.wet {
            let n = w.top.len();
            for (what, vals) in [("top", &w.top), ("bottom", &w.bottom)] {
                let vals: Vec<f64> = vals.iter().map(|&x| x as f64).collect();
                put(
                    sink,
                    one_d(
                        &format!("{}_wet_{what}", w.place),
                        &w.place,
                        n,
                        Kind::I32,
                        vec![text(
                            "long_name",
                            format!("{what} of the wet mid-levels (0-based, half open)"),
                        )],
                    ),
                    &vals,
                )?;
            }
        }
    }
    Ok(())
}

fn write_place(p: &Place, face_place: Option<&str>, sink: &mut dyn UgridSink) -> Result<()> {
    let n = p.size;
    let loc = if p.name == "nodes" {
        "node"
    } else if face_place == Some(p.name.as_str()) {
        "face"
    } else {
        ""
    };
    let common = |sn: &str, units: &str| {
        let mut a = vec![
            text("standard_name", sn),
            text("units", units),
            text("mesh", "mesh"),
        ];
        if !loc.is_empty() {
            a.push(text("location", loc));
        }
        a
    };
    put(
        sink,
        one_d(
            &format!("{}_lon", p.name),
            &p.name,
            n,
            Kind::F64,
            common("longitude", "degrees_east"),
        ),
        &p.lon,
    )?;
    put(
        sink,
        one_d(
            &format!("{}_lat", p.name),
            &p.name,
            n,
            Kind::F64,
            common("latitude", "degrees_north"),
        ),
        &p.lat,
    )?;
    put(
        sink,
        one_d(
            &format!("{}_area", p.name),
            &p.name,
            n,
            Kind::F64,
            common("cell_area", "m2"),
        ),
        &p.area,
    )?;
    if let Some(l) = &p.area_per_level {
        let name = format!("{}_area_per_level", p.name);
        sink.create(&ArraySpec {
            name: name.clone(),
            dims: vec![(format!("{}_nlev", p.name), l.nlev()), (p.name.clone(), n)],
            chunks: vec![1, n.max(1)],
            kind: Kind::F64,
            attrs: vec![
                text("long_name", "cell areas per mid-level"),
                text("units", "m2"),
            ],
        })?;
        for k in 0..l.nlev() {
            sink.write(&name, k, &l.level_uncached(k)?)?;
        }
    }
    if let Some(poly) = &p.polygons {
        let poly = poly.get_uncached()?;
        let offs: Vec<f64> = poly.offsets.iter().map(|&o| o as f64).collect();
        let pn = format!("{}_poly_n", p.name);
        let pv = format!("{}_poly_nv", p.name);
        put(
            sink,
            one_d(
                &format!("{}_poly_offsets", p.name),
                &pn,
                offs.len(),
                Kind::I32,
                vec![],
            ),
            &offs,
        )?;
        put(
            sink,
            one_d(
                &format!("{}_poly_lon", p.name),
                &pv,
                poly.lon.len(),
                Kind::F64,
                vec![],
            ),
            &poly.lon,
        )?;
        put(
            sink,
            one_d(
                &format!("{}_poly_lat", p.name),
                &pv,
                poly.lat.len(),
                Kind::F64,
                vec![],
            ),
            &poly.lat,
        )?;
    }
    Ok(())
}

fn write_table(mesh: &Mesh, t: &Table, sink: &mut dyn UgridSink) -> Result<()> {
    let csr = t.csr.get_uncached()?;
    let rows = csr.rows();
    let width = (0..rows).map(|r| csr.row(r).len()).max().unwrap_or(0);
    let mut vals = vec![-1.0; rows * width];
    for r in 0..rows {
        for (k, &c) in csr.row(r).iter().enumerate() {
            vals[r * width + k] = c as f64;
        }
    }
    let mut attrs = vec![
        num("_FillValue", -1.0),
        num("start_index", 0.0),
        text("cdors_rows", t.rows_place.clone()),
        text("cdors_cols", t.cols_place.clone()),
        num("cdors_nnz", csr.nnz() as f64),
    ];
    if mesh.place(&t.rows_place).is_some() && t.cols_place == "nodes" && width == 3 {
        attrs.push(text("cf_role", "face_node_connectivity"));
    }
    put(
        sink,
        ArraySpec {
            name: t.name.clone(),
            dims: vec![
                (t.rows_place.clone(), rows),
                (format!("{}_width", t.name), width),
            ],
            chunks: vec![rows.max(1), width.max(1)],
            kind: Kind::I32,
            attrs,
        },
        &vals,
    )
}

fn text_attr(r: &dyn ArrayReader, var: Option<&str>, name: &str) -> Option<String> {
    r.attr(var, name)
        .and_then(|a| a.as_str().map(str::to_owned))
}

fn words(r: &dyn ArrayReader, name: &str) -> Vec<String> {
    text_attr(r, Some("mesh"), name)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Rebuilds a mesh written by [`write`]. Centres, 2-D areas, the vertical and wet levels are
/// read now; per-level areas (level by level), polygons and tables load on first use through
/// `r`.
pub fn read(r: Arc<dyn ArrayReader>) -> Result<Mesh> {
    let file = r.describe();
    let format = r.attr(None, "cdors_mesh_format").and_then(|a| a.as_f64());
    if format != Some(FORMAT as f64) {
        return Err(MeshError::Invalid(format!(
            "{file}: not a cdors mesh of format {FORMAT} (cdors_mesh_format {format:?})"
        )));
    }
    let lines = |name: &str| -> Vec<String> {
        text_attr(&*r, None, name)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let source = Source {
        model: text_attr(&*r, None, "cdors_model").unwrap_or_default(),
        files: lines("cdors_files")
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        units: text_attr(&*r, None, "cdors_units"),
        notes: lines("cdors_notes"),
    };
    let mut places = Vec::new();
    for name in words(&*r, "cdors_places") {
        let lon = r.read(&format!("{name}_lon"))?;
        let lat = r.read(&format!("{name}_lat"))?;
        let area = r.read(&format!("{name}_area"))?;
        let mut p = Place::new(&name, lon, lat, area)?;
        let n = p.size;
        let apl = format!("{name}_area_per_level");
        if let Some(info) = r.var(&apl) {
            let rr = r.clone();
            p.area_per_level = Some(Levels::deferred(info.shape[0], n, move |k| {
                rr.read_range(&apl, &[k..k + 1, 0..n])
            }));
        }
        let offs = format!("{name}_poly_offsets");
        if let Some(info) = r.var(&offs) {
            let nv = r.var(&format!("{name}_poly_lon")).map_or(0, |v| v.len());
            let bytes = (8 * info.len() + 16 * nv) as u64;
            let (rr, name) = (r.clone(), name.clone());
            p.polygons = Some(Lazy::deferred(bytes, move || {
                Ok(Polygons {
                    offsets: rr.read(&offs)?.into_iter().map(|x| x as usize).collect(),
                    lon: rr.read(&format!("{name}_poly_lon"))?,
                    lat: rr.read(&format!("{name}_poly_lat"))?,
                })
            }));
        }
        places.push(p);
    }
    let mut topology = Vec::new();
    for name in words(&*r, "cdors_tables") {
        let info = r.require(&name)?;
        let rows_place = text_attr(&*r, Some(&name), "cdors_rows").unwrap_or_default();
        let cols_place = text_attr(&*r, Some(&name), "cdors_cols").unwrap_or_default();
        let ncols = places
            .iter()
            .find(|p| p.name == cols_place)
            .map(|p| p.size)
            .ok_or_else(|| {
                MeshError::Invalid(format!(
                    "{file}: table '{name}' into unknown place '{cols_place}'"
                ))
            })?;
        let (rows, width) = (info.shape[0], info.shape.get(1).copied().unwrap_or(0));
        let nnz = r
            .attr(Some(&name), "cdors_nnz")
            .and_then(|a| a.as_f64())
            .map_or(rows * width, |x| x as usize);
        let (rr, var) = (r.clone(), name.clone());
        let csr = Lazy::deferred(csr_bytes(rows, nnz, false), move || {
            let v = rr.read(&var)?;
            let mut row_ptr = Vec::with_capacity(rows + 1);
            row_ptr.push(0);
            let mut col = Vec::with_capacity(nnz);
            for row in v.chunks(width.max(1)).take(rows) {
                // padding: −1, or NaN where the reader masks the fill value
                col.extend(row.iter().filter(|x| **x >= 0.0).map(|&x| x as u32));
                row_ptr.push(col.len());
            }
            Csr::new(row_ptr, col, None, ncols)
        });
        topology.push(Table {
            name,
            rows_place,
            cols_place,
            csr,
        });
    }
    let vertical = match r.var("cdors_mid") {
        None => None,
        Some(_) => {
            let thickness =
                match text_attr(&*r, Some("cdors_interfaces"), "cdors_thickness").as_deref() {
                    Some("static") | None => Thickness::Static,
                    Some(t) => {
                        return Err(MeshError::Invalid(format!(
                            "{file}: unknown thickness '{t}'"
                        )));
                    }
                };
            let mut wet = Vec::new();
            for p in &places {
                let (t, b) = (
                    format!("{}_wet_top", p.name),
                    format!("{}_wet_bottom", p.name),
                );
                if r.var(&t).is_some() {
                    let u16s = |v: Vec<f64>| v.into_iter().map(|x| x as u16).collect();
                    wet.push(WetLevels {
                        place: p.name.clone(),
                        top: u16s(r.read(&t)?),
                        bottom: u16s(r.read(&b)?),
                    });
                }
            }
            Some(Vertical {
                interfaces: r.read("cdors_interfaces")?,
                mid: r.read("cdors_mid")?,
                thickness,
                wet,
            })
        }
    };
    Ok(Mesh {
        name: text_attr(&*r, None, "cdors_name").unwrap_or_default(),
        places,
        topology,
        vertical,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemReader, VarInfo};
    use std::collections::HashMap;

    /// A sink that keeps everything, readable back through [`MemReader`].
    #[derive(Default)]
    struct Mem {
        r: MemReader,
        specs: HashMap<String, ArraySpec>,
    }

    impl UgridSink for Mem {
        fn global(&mut self, attrs: &[(String, Attr)]) -> Result<()> {
            for (k, v) in attrs {
                self.r.attrs.insert((String::new(), k.clone()), v.clone());
            }
            Ok(())
        }
        fn create(&mut self, s: &ArraySpec) -> Result<()> {
            let info = VarInfo {
                dims: s.dims.iter().map(|d| d.0.clone()).collect(),
                shape: s.dims.iter().map(|d| d.1).collect(),
            };
            let n = info.len();
            for (d, n) in &s.dims {
                self.r.dims.insert(d.clone(), *n);
            }
            for (k, v) in &s.attrs {
                self.r.attrs.insert((s.name.clone(), k.clone()), v.clone());
            }
            self.r
                .vars
                .insert(s.name.clone(), (info, vec![f64::NAN; n.max(1)]));
            self.specs.insert(s.name.clone(), s.clone());
            Ok(())
        }
        fn write(&mut self, name: &str, first: usize, values: &[f64]) -> Result<()> {
            let (info, data) = self.r.vars.get_mut(name).unwrap();
            let row: usize = info.shape.iter().skip(1).product();
            data[first * row..first * row + values.len()].copy_from_slice(values);
            Ok(())
        }
    }

    #[test]
    fn round_trip() {
        // two triangles on four nodes, two layers, per-level node areas, wet levels
        let nodes = {
            let mut p = Place::new(
                "nodes",
                vec![0.0, 1.0, 0.0, 1.0],
                vec![0.0, 0.0, 1.0, 1.0],
                vec![1.0, 2.0, 3.0, 4.0],
            )
            .unwrap();
            p.area_per_level = Some(Levels::deferred(2, 4, |k| {
                Ok((0..4).map(|i| (10 * k + i) as f64).collect())
            }));
            p
        };
        let elems = Place::new("elements", vec![0.3, 0.7], vec![0.3, 0.7], vec![5.0, 6.0]).unwrap();
        let en = Csr::fixed(3, vec![0, 1, 2, 2, 1, 3], 4).unwrap();
        let mesh = Mesh {
            name: "test".into(),
            places: vec![nodes, elems],
            topology: vec![Table {
                name: "elem_nodes".into(),
                rows_place: "elements".into(),
                cols_place: "nodes".into(),
                csr: Lazy::ready(en.clone(), en.bytes()),
            }],
            vertical: Some(Vertical {
                interfaces: vec![0.0, -10.0, -30.0],
                mid: vec![-5.0, -20.0],
                thickness: Thickness::Static,
                wet: vec![WetLevels {
                    place: "nodes".into(),
                    top: vec![0, 0, 1, 0],
                    bottom: vec![2, 1, 2, 2],
                }],
            }),
            source: Source {
                model: "fesom2".into(),
                files: vec!["/a/fesom.mesh.diag.nc".into()],
                units: Some("radians".into()),
                notes: vec!["one".into(), "two".into()],
            },
        };
        let mut sink = Mem::default();
        write(&mesh, &[text("cdors_fingerprint", "abc")], &mut sink).unwrap();
        assert_eq!(sink.specs["nodes_area_per_level"].chunks, vec![1, 4]);
        let back = read(Arc::new(sink.r)).unwrap();
        assert_eq!(back.name, "test");
        assert_eq!(back.source, mesh.source);
        for (a, b) in mesh.places.iter().zip(&back.places) {
            assert_eq!(
                (&a.name, &a.lon, &a.lat, &a.area),
                (&b.name, &b.lon, &b.lat, &b.area)
            );
        }
        let l = back.places[0].area_per_level.as_ref().unwrap();
        assert!(!l.levels[1].is_loaded());
        assert_eq!(*l.level(1).unwrap(), vec![10.0, 11.0, 12.0, 13.0]);
        assert!(back.places[1].area_per_level.is_none());
        let t = back.table("elem_nodes").unwrap();
        assert_eq!(
            (t.rows_place.as_str(), t.cols_place.as_str()),
            ("elements", "nodes")
        );
        assert!(!t.csr.is_loaded());
        assert_eq!(*t.csr.get().unwrap(), en);
        assert_eq!(back.vertical, mesh.vertical);
        assert_eq!(back.resident_bytes(), mesh.resident_bytes());
        assert_eq!(back.lazy_bytes().0, mesh.lazy_bytes().0);
    }
}
