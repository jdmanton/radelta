//! Resolve the planes belonging to the supplied file, never sibling pixels.
use super::{axis_coord, axis_dim, Dims5, InferredLayout};
use roxmltree::Node;
use std::{collections::HashSet, path::Path};

fn usize_attr(node: Node<'_, '_>, name: &str) -> Result<Option<usize>, String> {
    node.attribute(name)
        .map(|v| v.parse().map_err(|_| format!("invalid OME {name}=\"{v}\"")))
        .transpose()
}

fn is_local(data: Node<'_, '_>, root_uuid: Option<&str>, path: &Path) -> Result<bool, String> {
    let root_uuid = root_uuid.map(str::trim).filter(|uuid| !uuid.is_empty());
    let mut uuids = data.children().filter(|n| n.tag_name().name() == "UUID");
    let Some(uuid) = uuids.next() else {
        return Ok(true); // No UUID means the enclosing TIFF.
    };
    if uuids.next().is_some() {
        return Err("OME TiffData has more than one UUID reference".into());
    }
    if let (Some(root), Some(reference)) = (root_uuid, uuid.text().map(str::trim)) {
        if !reference.is_empty() {
            return Ok(root.eq_ignore_ascii_case(reference));
        }
    }
    if let Some(name) = uuid.attribute("FileName") {
        // Resolve relative to this TIFF, not the process working directory.
        // Do not identify a different directory's same-named TIFF as local.
        let named = path.parent().unwrap_or(Path::new(".")).join(name);
        return Ok(named == path
            || named
                .canonicalize()
                .ok()
                .zip(path.canonicalize().ok())
                .is_some_and(|(a, b)| a == b));
    }
    Err("Cannot identify this TIFF's OME TiffData: provide an OME root UUID or UUID FileName; use --ignore-metadata only with a known explicit --t/--c/--z layout".into())
}

fn linear(t: usize, c: usize, z: usize, order: &str, d: Dims5) -> usize {
    let mut index = 0;
    let mut stride = 1;
    for axis in order.bytes() {
        index += axis_coord(axis, t, c, z) * stride;
        stride *= axis_dim(axis, d);
    }
    index
}

fn coordinates(mut index: usize, order: &str, d: Dims5) -> (usize, usize, usize) {
    let (mut t, mut c, mut z) = (0, 0, 0);
    for axis in order.bytes() {
        let value = index % axis_dim(axis, d);
        index /= axis_dim(axis, d);
        match axis {
            b'T' => t = value,
            b'C' => c = value,
            b'Z' => z = value,
            _ => unreachable!(),
        }
    }
    (t, c, z)
}

pub(super) fn infer_ome_layout(
    desc: &str,
    x: usize,
    y: usize,
    pages: usize,
    path: &Path,
) -> Result<Option<InferredLayout>, String> {
    if !desc.contains("<OME") && !desc.contains(":OME") {
        return Ok(None);
    }
    let doc = roxmltree::Document::parse(desc).map_err(|e| format!("Invalid OME-XML: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "OME" {
        return Ok(None);
    }
    let mut images = root.children().filter(|n| n.tag_name().name() == "Image");
    let image = images
        .next()
        .ok_or("OME metadata has no Image/Pixels; external BinaryOnly metadata is not supported")?;
    if images.next().is_some() {
        return Err(
            "OME metadata contains multiple Images; select a single image series before encoding"
                .into(),
        );
    }
    let pixels = image
        .children()
        .find(|n| n.tag_name().name() == "Pixels")
        .ok_or("OME Image missing Pixels")?;
    let sx = usize_attr(pixels, "SizeX")?.ok_or("OME Pixels missing SizeX")?;
    let sy = usize_attr(pixels, "SizeY")?.ok_or("OME Pixels missing SizeY")?;
    let z = usize_attr(pixels, "SizeZ")?.ok_or("OME Pixels missing SizeZ")?;
    let c = usize_attr(pixels, "SizeC")?.ok_or("OME Pixels missing SizeC")?;
    let t = usize_attr(pixels, "SizeT")?.ok_or("OME Pixels missing SizeT")?;
    if sx != x || sy != y {
        return Err(format!(
            "OME SizeX/SizeY={sx}x{sy}, but TIFF pages are {x}x{y}"
        ));
    }
    if z == 0 || c == 0 || t == 0 || pages == 0 {
        return Err("OME SizeZ/SizeC/SizeT and TIFF plane count must be positive".into());
    }
    let product = z
        .checked_mul(c)
        .and_then(|n| n.checked_mul(t))
        .ok_or("OME T*C*Z overflow")?;
    let order = pixels
        .attribute("DimensionOrder")
        .ok_or("OME Pixels missing DimensionOrder")?
        .to_ascii_uppercase();
    if order.len() != 5 || !order.starts_with("XY") {
        return Err(format!(
            "unsupported OME DimensionOrder={order}; expected XY followed by a permutation of ZCT"
        ));
    }
    let axes = &order[2..];
    let mut sorted = axes.as_bytes().to_vec();
    sorted.sort_unstable();
    if sorted.as_slice() != b"CTZ" {
        return Err(format!("unsupported OME DimensionOrder={order}"));
    }
    let page_order = axes.chars().rev().collect();
    let dims = Dims5 { x, y, z, c, t };
    let entries: Vec<_> = pixels
        .children()
        .filter(|n| n.tag_name().name() == "TiffData")
        .collect();
    let explicit = entries.len() > 1
        || entries.iter().any(|n| {
            n.attributes().len() != 0 || n.children().any(|n| n.tag_name().name() == "UUID")
        });
    if !explicit && product == pages {
        return Ok(Some(InferredLayout {
            t,
            c,
            z,
            page_order,
            source: "OME-TIFF".into(),
            ifd_coords: None,
        }));
    }
    if entries.is_empty() {
        return Err(format!("OME metadata describes T*C*Z={product} planes but this TIFF contains {pages} IFDs and no TiffData mapping identifies its subset"));
    }
    let mut map = vec![None; pages];
    for entry in entries {
        if !is_local(entry, root.attribute("UUID"), path)? {
            continue;
        }
        let ifd = usize_attr(entry, "IFD")?.unwrap_or(0);
        let ft = usize_attr(entry, "FirstT")?.unwrap_or(0);
        let fc = usize_attr(entry, "FirstC")?.unwrap_or(0);
        let fz = usize_attr(entry, "FirstZ")?.unwrap_or(0);
        let count =
            usize_attr(entry, "PlaneCount")?.unwrap_or(if entry.attribute("IFD").is_some() {
                1
            } else {
                pages
            });
        if ft >= t || fc >= c || fz >= z {
            return Err("OME TiffData FirstT/FirstC/FirstZ lies outside Pixels dimensions".into());
        }
        let start = linear(ft, fc, fz, axes, dims);
        if ifd.checked_add(count).is_none_or(|end| end > pages)
            || start.checked_add(count).is_none_or(|end| end > product)
        {
            return Err("OME TiffData mapping exceeds this TIFF or Pixels dimensions".into());
        }
        for j in 0..count {
            if map[ifd + j]
                .replace(coordinates(start + j, axes, dims))
                .is_some()
            {
                return Err(format!("OME TiffData maps IFD {} more than once", ifd + j));
            }
        }
    }
    let coords = map.into_iter().collect::<Option<Vec<_>>>()
        .ok_or("OME TiffData mapping does not cover every IFD in this TIFF; file UUID/FileName or plane mappings may be missing")?;
    let unique: HashSet<_> = coords.iter().copied().collect();
    if unique.len() != pages {
        return Err("OME TiffData maps multiple IFDs to the same TCZ plane".into());
    }
    let (mut lo, mut hi) = ([usize::MAX; 3], [0; 3]);
    for &(t, c, z) in &coords {
        for (i, value) in [t, c, z].into_iter().enumerate() {
            lo[i] = lo[i].min(value);
            hi[i] = hi[i].max(value);
        }
    }
    let [nt, nc, nz] = std::array::from_fn(|i| hi[i] - lo[i] + 1);
    if nt.checked_mul(nc).and_then(|n| n.checked_mul(nz)) != Some(pages) {
        return Err("This TIFF's OME planes do not form a complete contiguous TCZ subset; refusing to fill missing planes or collapse gaps".into());
    }
    let partial = product != pages;
    Ok(Some(InferredLayout {
        t: nt,
        c: nc,
        z: nz,
        page_order,
        source: if partial {
            format!("OME-TIFF (this file's subset: source T={}..={}, C={}..={}, Z={}..={}; acquisition T={t}, C={c}, Z={z})", lo[0], hi[0], lo[1], hi[1], lo[2], hi[2])
        } else {
            "OME-TIFF (explicit TiffData mapping)".into()
        },
        ifd_coords: Some(
            coords
                .into_iter()
                .map(|(t, c, z)| (t - lo[0], c - lo[1], z - lo[2]))
                .collect(),
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml(t: usize, c: usize, z: usize, entries: &str) -> String {
        format!(
            r#"<o:OME xmlns:o="urn:ome" UUID="local"><o:Image><o:Pixels DimensionOrder="XYZCT" SizeX="10" SizeY="7" SizeZ="{z}" SizeC="{c}" SizeT="{t}">{entries}</o:Pixels></o:Image></o:OME>"#
        )
    }

    fn infer(xml: &str, pages: usize) -> Result<InferredLayout, String> {
        infer_ome_layout(xml, 10, 7, pages, Path::new("lung & sample_C02.ome.tif"))?
            .ok_or("not OME".into())
    }

    #[test]
    fn single_channel_of_four_1599_plane_regression() {
        for channel in 0..4 {
            let mut entries = String::new();
            for c in 0..4 {
                let uuid = if c == channel {
                    "local".into()
                } else {
                    format!("foreign-{c}")
                };
                entries.push_str(&format!(r#"<o:TiffData IFD="0" FirstC="{c}" PlaneCount="1599"><o:UUID FileName="missing_C{c}.ome.tif">{uuid}</o:UUID></o:TiffData>"#));
            }
            let m = infer(&xml(1, 4, 1599, &entries), 1599).unwrap();
            assert_eq!((m.t, m.c, m.z), (1, 1, 1599));
            let coords = m.ifd_coords.unwrap();
            assert_eq!(coords[0], (0, 0, 0));
            assert_eq!(coords[1598], (0, 0, 1598));
            assert!(m.source.contains(&format!("C={channel}..={channel}")));
        }
    }

    #[test]
    fn local_rectangle_and_reverse_ifds_in_every_dimension_order() {
        let coords: Vec<_> = (1..3)
            .flat_map(|t| (1..3).flat_map(move |c| (2..5).map(move |z| (t, c, z))))
            .rev()
            .collect();
        let entries: String = coords
            .iter()
            .enumerate()
            .map(|(p, (t, c, z))| {
                format!(r#"<o:TiffData IFD="{p}" FirstT="{t}" FirstC="{c}" FirstZ="{z}"/>"#)
            })
            .collect();
        for order in ["ZCT", "ZTC", "CZT", "CTZ", "TCZ", "TZC"] {
            let text = xml(3, 4, 5, &entries).replace("XYZCT", &format!("XY{order}"));
            let m = infer(&text, coords.len()).unwrap();
            assert_eq!((m.t, m.c, m.z), (2, 2, 3));
            assert_eq!(m.page_order, order.chars().rev().collect::<String>());
            assert_eq!(
                m.ifd_coords.unwrap(),
                coords
                    .iter()
                    .map(|&(t, c, z)| (t - 1, c - 1, z - 2))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn filename_fallback_decodes_xml_entities_and_ignores_foreign_files() {
        let text = xml(1, 4, 3, r#"<o:TiffData IFD="0" FirstC="2" PlaneCount="3"><o:UUID FileName="lung &amp; sample_C02.ome.tif">local</o:UUID></o:TiffData><o:TiffData IFD="0" FirstC="1" PlaneCount="3"><o:UUID FileName="elsewhere/lung &amp; sample_C02.ome.tif">foreign</o:UUID></o:TiffData>"#).replace(" UUID=\"local\"", "");
        let m = infer(&text, 3).unwrap();
        assert_eq!((m.t, m.c, m.z), (1, 1, 3));
        assert!(m.source.contains("C=2..=2"));
    }

    #[test]
    fn implicit_local_range_uses_ome_defaults() {
        let m = infer(&xml(1, 4, 3, "<o:TiffData/>"), 3).unwrap();
        assert_eq!((m.t, m.c, m.z), (1, 1, 3));
        let m = infer(&xml(1, 4, 3, r#"<o:TiffData FirstC="2"/>"#), 3).unwrap();
        assert!(m.source.contains("C=2..=2"));
    }

    #[test]
    fn ambiguous_incomplete_duplicate_and_gapped_mappings_fail() {
        assert!(infer(&xml(1, 1, 2, "<o:TiffData/><o:TiffData/>"), 2).is_err());
        for entries in [
            "",                         // A global plane count alone does not identify this file.
            r#"<o:TiffData IFD="0"/>"#, // Missing second IFD.
            r#"<o:TiffData IFD="0"/><o:TiffData IFD="0" FirstZ="1"/>"#,
            r#"<o:TiffData IFD="0"/><o:TiffData IFD="1"/>"#, // Duplicate TCZ.
            r#"<o:TiffData IFD="0"/><o:TiffData IFD="1" FirstZ="2"/>"#, // Gap.
            r#"<o:TiffData IFD="0" PlaneCount="3"/>"#,       // Too many local IFDs.
            r#"<o:TiffData IFD="0" FirstC="4" PlaneCount="2"/>"#,
            r#"<o:TiffData IFD="0" FirstC="3" FirstZ="2" PlaneCount="2"/>"#,
            r#"<o:TiffData IFD="0" PlaneCount="2"><o:UUID>foreign</o:UUID></o:TiffData>"#,
        ] {
            assert!(infer(&xml(1, 4, 3, entries), 2).is_err(), "{entries}");
        }
        let ambiguous = xml(
            1,
            4,
            3,
            r#"<o:TiffData PlaneCount="2"><o:UUID>unknown</o:UUID></o:TiffData>"#,
        )
        .replace(" UUID=\"local\"", "");
        assert!(infer(&ambiguous, 2)
            .unwrap_err()
            .contains("Cannot identify"));
    }

    #[test]
    fn comments_and_other_pixels_are_not_plane_mappings() {
        let text = xml(
            1,
            4,
            3,
            r#"<!-- <o:TiffData IFD="0" PlaneCount="99"/> --><o:TiffData FirstC="1"/>"#,
        );
        assert_eq!(infer(&text, 3).unwrap().c, 1);
        assert!(infer(
            "<OME><BinaryOnly MetadataFile=\"missing.ome.xml\"/></OME>",
            3
        )
        .is_err());
        assert!(infer(&text.replace("</o:OME>", "<o:Image/></o:OME>"), 3)
            .unwrap_err()
            .contains("multiple Images"));
    }
}
