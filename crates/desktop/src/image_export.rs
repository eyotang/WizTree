//! Export a selected subtree as a PNG area map, without text labels.
//!
//! The image uses the same hierarchy, metric, extension colors and conservative
//! 6000-tile aggregation as the interactive map. It exports the complete selected
//! scope, independent of the table's search filters and row limit. Directory
//! aggregates are neutral gray. Default UI exports are intended to be 1600x900.
use crate::{query, treemap};
use disk_core::Snapshot;
use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
};

const MAX_DIMENSION: u32 = 8192;
const DIRECTORY_COLOR: u32 = 0x8496aa;

/// Write an opaque, 8-bit RGB PNG of the selected subtree.
///
/// Both dimensions must be in 1..=8192. Invalid dimensions, a missing root or
/// a zero-size selected metric are rejected before the destination is opened.
/// Shared rectangle boundaries are snapped together before integer rasterizing,
/// so floating-point rounding cannot create gaps between adjacent rectangles.
/// One-pixel white separators are drawn inside the rectangles; there are no
/// filename labels, legends or UI chrome in this area-map export.
pub fn save_png(
    snapshot: &Snapshot,
    root: usize,
    allocated: bool,
    path: &Path,
    width: u32,
    height: u32,
) -> io::Result<()> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PNG 尺寸必须在 1 到 8192 像素之间",
        ));
    }
    let node = snapshot
        .nodes
        .get(root)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "导出目录不存在"))?;
    if (if allocated { node.allocated } else { node.size }) == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "所选目录没有可绘制的空间占用",
        ));
    }
    let tiles =
        treemap::hierarchical_layout(snapshot, root, allocated, width as f32, height as f32, 6000);
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "PNG 尺寸溢出"))?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(len).map_err(io::Error::other)?;
    pixels.resize(len, 255);
    let horizontal = snap_axis(&tiles, width, true);
    let vertical = snap_axis(&tiles, height, false);
    for (index, tile) in tiles.iter().enumerate() {
        let Some(node) = tile.id.and_then(|id| snapshot.nodes.get(id)) else {
            continue;
        };
        let color = if node.is_dir {
            DIRECTORY_COLOR
        } else {
            query::color(&query::extension(&node.name))
        };
        let rgb = [(color >> 16) as u8, (color >> 8) as u8, color as u8];
        let (left, right) = horizontal[index];
        let (top, bottom) = vertical[index];
        if left >= right || top >= bottom {
            continue;
        }
        for y in top..bottom {
            let row_start = (y as usize * width as usize + left as usize) * 3;
            let row_end = (y as usize * width as usize + right as usize) * 3;
            let row = &mut pixels[row_start..row_end];
            if y + 1 == bottom || y == 0 {
                continue;
            }
            for pixel in row.chunks_exact_mut(3) {
                pixel.copy_from_slice(&rgb);
            }
            // Draw each shared edge once, on its top/left owner's right/bottom.
            let last_pixel = row.len() - 3;
            row[last_pixel..].fill(255);
            if left == 0 {
                row[..3].fill(255);
            }
        }
    }
    let mut output = BufWriter::new(File::create(path)?);
    {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_source_srgb(png::SrgbRenderingIntent::RelativeColorimetric);
        encoder.add_text_chunk("Description".into(), format!(
            "File-space area map; metric={}; colors=file extension; small files may be grouped into directory tiles; no text labels.",
            if allocated { "allocated bytes" } else { "logical bytes" }
        ))?;
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
        writer.finish()?;
    }
    output.flush()
}

/// Collapse numerically identical shared edges to one integer pixel boundary.
/// f32-normalized coordinates can differ by a few ulps when x+w is recomputed.
fn snap_axis(tiles: &[treemap::Tile], extent: u32, horizontal: bool) -> Vec<(u32, u32)> {
    let mut edges = Vec::with_capacity(tiles.len() * 2);
    for (index, tile) in tiles.iter().enumerate() {
        let (start, length) = if horizontal {
            (tile.x, tile.w)
        } else {
            (tile.y, tile.h)
        };
        edges.push((
            f64::from(start).clamp(0., 1.) * f64::from(extent),
            index,
            false,
        ));
        edges.push((
            (f64::from(start) + f64::from(length)).clamp(0., 1.) * f64::from(extent),
            index,
            true,
        ));
    }
    edges.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let tolerance = f64::from(extent) * f64::from(f32::EPSILON) * 4.;
    let mut bounds = vec![(0, 0); tiles.len()];
    let mut start = 0;
    while start < edges.len() {
        let mut end = start + 1;
        while end < edges.len() && edges[end].0 - edges[start].0 <= tolerance {
            end += 1;
        }
        let mean = edges[start..end].iter().map(|edge| edge.0).sum::<f64>() / (end - start) as f64;
        let pixel = mean.round().clamp(0., f64::from(extent)) as u32;
        for &(_, index, ending) in &edges[start..end] {
            if ending {
                bounds[index].1 = pixel;
            } else {
                bounds[index].0 = pixel;
            }
        }
        start = end;
    }
    bounds
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::{Node, ScanMethod};
    use std::{
        io::Cursor,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };
    struct Output(PathBuf);
    impl Output {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            Self(std::env::temp_dir().join(format!(
                "wiztree-png-{}-{}.png",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }
    impl Drop for Output {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn fixture() -> Snapshot {
        let node = |name: &str, size, allocated, parent, children: Vec<usize>| Node {
            parent,
            name: name.into(),
            size,
            allocated,
            is_dir: !children.is_empty(),
            modified: None,
            children,
        };
        Snapshot {
            root: "test".into(),
            nodes: vec![
                node("test", 100, 200, None, vec![1, 2]),
                node("selected", 20, 40, Some(0), vec![3, 4]),
                node("outside.bin", 80, 160, Some(0), vec![]),
                node("a.txt", 12, 24, Some(1), vec![]),
                node("b.zip", 8, 16, Some(1), vec![]),
            ],
            elapsed: Duration::ZERO,
            files: 3,
            dirs: 2,
            errors: 0,
            cancelled: false,
            method: ScanMethod::ParallelWalk,
            fallback_reason: None,
            issues: Vec::new(),
        }
    }
    #[test]
    fn png_signature_dimensions_and_real_content() {
        let output = Output::new();
        save_png(&fixture(), 1, true, &output.0, 160, 90).unwrap();
        let encoded = std::fs::read(&output.0).unwrap();
        assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
        let mut reader = png::Decoder::new(Cursor::new(encoded)).read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!(
            (info.width, info.height, info.color_type),
            (160, 90, png::ColorType::Rgb)
        );
        let colors: Vec<_> = ["txt", "zip"]
            .into_iter()
            .map(|ext| {
                let n = query::color(ext);
                [(n >> 16) as u8, (n >> 8) as u8, n as u8]
            })
            .collect();
        let pixels = &pixels[..info.buffer_size()];
        assert_eq!(&pixels[..3], &[255, 255, 255]);
        for color in colors {
            assert!(pixels.chunks_exact(3).any(|pixel| pixel == color));
        }
        assert!(
            pixels
                .chunks_exact(3)
                .filter(|pixel| *pixel != [255, 255, 255])
                .count()
                > 13_000
        );
    }
    #[test]
    fn invalid_dimensions_and_roots_do_not_create_destination() {
        let output = Output::new();
        let mut snapshot = fixture();
        for (width, height) in [
            (0, 90),
            (160, 0),
            (8193, 90),
            (160, 8193),
            (u32::MAX, u32::MAX),
        ] {
            assert_eq!(
                save_png(&snapshot, 0, false, &output.0, width, height)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            save_png(&snapshot, usize::MAX, false, &output.0, 160, 90)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        snapshot.nodes[0].size = 0;
        assert_eq!(
            save_png(&snapshot, 0, false, &output.0, 160, 90)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!output.0.exists());
    }
    #[test]
    fn shared_edges_snap_to_identical_pixel_boundaries() {
        let tiles = [
            treemap::Tile {
                id: Some(0),
                bytes: 1,
                x: 0.,
                y: 0.,
                w: 0.33333334,
                h: 1.,
            },
            treemap::Tile {
                id: Some(1),
                bytes: 2,
                x: 0.3333333,
                y: 0.,
                w: 0.6666667,
                h: 1.,
            },
        ];
        let bounds = snap_axis(&tiles, 8192, true);
        assert_eq!(bounds[0].0, 0);
        assert_eq!(bounds[0].1, bounds[1].0);
        assert_eq!(bounds[1].1, 8192);
    }
}
