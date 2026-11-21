//! Fast image processing, demosaicing, and MTF Auto-Stretch engine for FulgurPreView.

pub mod debayer;
pub mod field;
pub mod histogram;
pub mod levels;
pub mod sky;
pub mod stars;
pub mod stretch;
pub mod vng;

#[cfg(test)]
mod tests {
    use crate::stretch::{
        ChannelView, StretchMode, StretchParams, compute_auto_stretch, load_detail_roi_rgb8,
        load_frame_overview,
    };
    use fulgur_core::{LayoutChoice, Mosaic};
    use std::io::Write;
    use std::sync::atomic::AtomicBool;

    fn make_fits_card(key: &str, val: &str) -> [u8; 80] {
        let mut card = [b' '; 80];
        let text = if key == "END" {
            "END".to_owned()
        } else {
            format!("{key:<8}= {val:>20}")
        };
        let bytes = text.as_bytes();
        card[..bytes.len().min(80)].copy_from_slice(&bytes[..bytes.len().min(80)]);
        card
    }

    fn write_header_block(cards: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(k, v) in cards {
            out.extend_from_slice(&make_fits_card(k, v));
        }
        out.extend_from_slice(&make_fits_card("END", ""));
        while out.len() % 2880 != 0 {
            out.push(b' ');
        }
        out
    }

    fn pad_data_block(mut data: Vec<u8>) -> Vec<u8> {
        while data.len() % 2880 != 0 {
            data.push(0);
        }
        data
    }

    #[test]
    fn test_fits_all_bitpix_and_mef() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);

        // 1. BITPIX = 8
        {
            let path = dir.path().join("bitpix8.fits");
            let mut bytes = write_header_block(&[
                ("SIMPLE", "T"),
                ("BITPIX", "8"),
                ("NAXIS", "2"),
                ("NAXIS1", "16"),
                ("NAXIS2", "16"),
            ]);
            let data: Vec<u8> = (0..256).map(|i| (i % 256) as u8).collect();
            bytes.extend_from_slice(&pad_data_block(data));
            std::fs::write(&path, bytes).unwrap();

            let ov = load_frame_overview(&path, 0, LayoutChoice::Mono, &cancel).unwrap();
            assert_eq!(ov.native_size, [16, 16]);
            assert!(ov.default_stretch);
            assert_eq!(ov.info.bitpix, 8);
        }

        // 2. BITPIX = 16 with BZERO = 32768 and RGGB Bayer
        {
            let path = dir.path().join("bitpix16_bayer.fits");
            let mut bytes = write_header_block(&[
                ("SIMPLE", "T"),
                ("BITPIX", "16"),
                ("NAXIS", "2"),
                ("NAXIS1", "32"),
                ("NAXIS2", "32"),
                ("BZERO", "32768"),
                ("BSCALE", "1"),
                ("BAYERPAT", "'RGGB'"),
                ("EXPTIME", "10.0"),
            ]);
            let mut data = Vec::with_capacity(32 * 32 * 2);
            for i in 0..(32 * 32) {
                let adu: u16 = 1000 + (i as u16 % 4000);
                let stored = (adu as i32 - 32768) as i16;
                data.extend_from_slice(&stored.to_be_bytes());
            }
            bytes.extend_from_slice(&pad_data_block(data));
            std::fs::write(&path, bytes).unwrap();

            let ov = load_frame_overview(&path, 0, LayoutChoice::Auto, &cancel).unwrap();
            assert_eq!(ov.native_size, [32, 32]);
            assert_eq!(ov.info.mosaic, Mosaic::Bayer);
            let rgb8 = ov.render_rgb8(ov.resolve_stretch(
                StretchMode::AutoUnlinked,
                &StretchParams::identity(),
                false,
                ChannelView::Rgb,
            ));
            assert_eq!(rgb8.len(), 32 * 32 * 3);

            // Also verify 1:1 VNG detail ROI streaming on this Bayer FITS
            let detail = load_detail_roi_rgb8(
                &path,
                0,
                LayoutChoice::Auto,
                [32, 32],
                [4, 4, 16, 16],
                ov.norm_min,
                ov.norm_span,
                ov.unlinked_stretch,
                &cancel,
            )
            .unwrap();
            assert_eq!(detail.len(), 16 * 16 * 3);
        }

        // 3. BITPIX = 32 (signed 32-bit integer)
        {
            let path = dir.path().join("bitpix32.fits");
            let mut bytes = write_header_block(&[
                ("SIMPLE", "T"),
                ("BITPIX", "32"),
                ("NAXIS", "2"),
                ("NAXIS1", "16"),
                ("NAXIS2", "16"),
            ]);
            let mut data = Vec::with_capacity(16 * 16 * 4);
            for i in 0..256 {
                let v: i32 = i * 100;
                data.extend_from_slice(&v.to_be_bytes());
            }
            bytes.extend_from_slice(&pad_data_block(data));
            std::fs::write(&path, bytes).unwrap();

            let ov = load_frame_overview(&path, 0, LayoutChoice::Mono, &cancel).unwrap();
            assert_eq!(ov.native_size, [16, 16]);
            assert_eq!(ov.info.bitpix, 32);
        }

        // 4. BITPIX = -32 (3D RGB float cube)
        {
            let path = dir.path().join("bitpix_f32_rgb.fits");
            let mut bytes = write_header_block(&[
                ("SIMPLE", "T"),
                ("BITPIX", "-32"),
                ("NAXIS", "3"),
                ("NAXIS1", "16"),
                ("NAXIS2", "16"),
                ("NAXIS3", "3"),
            ]);
            let mut data = Vec::with_capacity(16 * 16 * 3 * 4);
            for c in 0..3 {
                for i in 0..256 {
                    let v: f32 = (i as f32 / 255.0) * (0.2 + 0.3 * c as f32);
                    data.extend_from_slice(&v.to_be_bytes());
                }
            }
            bytes.extend_from_slice(&pad_data_block(data));
            std::fs::write(&path, bytes).unwrap();

            let ov = load_frame_overview(&path, 0, LayoutChoice::Auto, &cancel).unwrap();
            assert_eq!(ov.native_size, [16, 16]);
            assert_eq!(ov.info.mosaic, Mosaic::Rgb);
        }

        // 5. BITPIX = -64 (64-bit IEEE float)
        {
            let path = dir.path().join("bitpix_f64.fits");
            let mut bytes = write_header_block(&[
                ("SIMPLE", "T"),
                ("BITPIX", "-64"),
                ("NAXIS", "2"),
                ("NAXIS1", "16"),
                ("NAXIS2", "16"),
            ]);
            let mut data = Vec::with_capacity(16 * 16 * 8);
            for i in 0..256 {
                let v: f64 = (i as f64) / 255.0;
                data.extend_from_slice(&v.to_be_bytes());
            }
            bytes.extend_from_slice(&pad_data_block(data));
            std::fs::write(&path, bytes).unwrap();

            let ov = load_frame_overview(&path, 0, LayoutChoice::Mono, &cancel).unwrap();
            assert_eq!(ov.native_size, [16, 16]);
            assert_eq!(ov.info.bitpix, -64);
        }

        // 6. Multi-Extension FITS (MEF): Primary HDU empty (NAXIS=0), Extension 1 has image
        {
            let path = dir.path().join("mef.fits");
            let mut bytes = write_header_block(&[
                ("SIMPLE", "T"),
                ("BITPIX", "8"),
                ("NAXIS", "0"),
                ("EXTEND", "T"),
            ]);
            bytes.extend_from_slice(&write_header_block(&[
                ("XTENSION", "'IMAGE   '"),
                ("BITPIX", "16"),
                ("NAXIS", "2"),
                ("NAXIS1", "20"),
                ("NAXIS2", "12"),
                ("EXTNAME", "'SCI     '"),
            ]));
            let mut data = Vec::with_capacity(20 * 12 * 2);
            for i in 0..(20 * 12) {
                data.extend_from_slice(&(i as i16).to_be_bytes());
            }
            bytes.extend_from_slice(&pad_data_block(data));
            std::fs::write(&path, bytes).unwrap();

            let ov = load_frame_overview(&path, 0, LayoutChoice::Mono, &cancel).unwrap();
            assert_eq!(ov.native_size, [20, 12]);
            assert_eq!(ov.hdu_names.len(), 1);
            assert!(ov.hdu_names[0].contains("SCI"));
        }
    }

    #[test]
    fn test_xisf_and_standard_formats() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);

        // 1. Uncompressed XISF
        {
            let path = dir.path().join("test.xisf");
            let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<xisf version="1.0">
  <Image geometry="8:8:1" sampleFormat="Float32" colorSpace="Gray" location="attachment:2048:256">
    <FITSKeyword name="OBJECT" value="'M42'" comment="Target"/>
    <FITSKeyword name="EXPTIME" value="30.0" comment="Exposure"/>
  </Image>
</xisf>"#;
            let xml_bytes = xml.as_bytes();
            let mut file_bytes = Vec::new();
            file_bytes.extend_from_slice(b"XISF0100");
            file_bytes.extend_from_slice(&(xml_bytes.len() as u32).to_le_bytes());
            file_bytes.extend_from_slice(&0u32.to_le_bytes());
            file_bytes.extend_from_slice(xml_bytes);
            file_bytes.resize(2048, 0);
            for i in 0..64 {
                file_bytes.extend_from_slice(&((i as f32) / 63.0).to_le_bytes());
            }
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&file_bytes).unwrap();

            let ov = load_frame_overview(&path, 0, LayoutChoice::Auto, &cancel).unwrap();
            assert_eq!(ov.native_size, [8, 8]);
            assert!(ov.default_stretch);
            assert_eq!(ov.info.object.as_deref(), Some("M42"));
        }

        // 2. Standard PNG (default_stretch must be FALSE)
        {
            let path = dir.path().join("photo.png");
            let img = image::RgbImage::from_fn(16, 12, |x, y| {
                image::Rgb([(x * 16) as u8, (y * 20) as u8, 128])
            });
            img.save(&path).unwrap();

            let ov = load_frame_overview(&path, 0, LayoutChoice::Auto, &cancel).unwrap();
            assert_eq!(ov.native_size, [16, 12]);
            assert!(!ov.default_stretch);
        }
    }

    #[test]
    fn test_linked_vs_unlinked_stretch() {
        // Strong green cast background: R=0.05, G=0.25, B=0.08
        let mut samples = Vec::with_capacity(1000);
        for i in 0..1000 {
            let jitter = (i as f32 % 10.0) * 0.001;
            samples.push([0.05 + jitter, 0.25 + jitter, 0.08 + jitter]);
        }
        let (linked, unlinked) = compute_auto_stretch(&samples);
        assert_eq!(linked.black[0], linked.black[1]);
        assert_eq!(linked.black[1], linked.black[2]);
        // Unlinked must set a higher black point on Green than on Red
        assert!(unlinked.black[1] > unlinked.black[0] + 0.10);
    }

    #[test]
    fn test_fast_cfa_overview_no_grid_lines_across_camera_dimensions() {
        use crate::debayer::fast_cfa_overview;
        use fulgur_core::fits::read_header;
        use fulgur_core::{BayerPattern, Mosaic, RawFrame};

        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let dummy_path = dir.path().join("dummy.fits");
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "16"),
            ("NAXIS2", "16"),
            ("BAYERPAT", "'GRBG'"),
        ]);
        bytes.extend_from_slice(&pad_data_block(vec![0u8; 512]));
        std::fs::write(&dummy_path, bytes).unwrap();
        let base_header = read_header(&dummy_path).unwrap();

        // Test matrix of diverse camera resolutions:
        // Seestar S50 (1080x1920), Seestar S30/S50 Pro (2160x3840), ASI533 (3008x3008),
        // odd crops (2049x2051, 4103x2735), ASI183 (5496x3672), ASI2600 (6248x4176)
        for (w, h) in [
            (1080_usize, 1920_usize),
            (2160, 3840),
            (2049, 2051),
            (3008, 3008),
            (4103, 2735),
            (5496, 3672),
            (6248, 4176),
        ] {
            let mut header = base_header.clone();
            header.info.width = w;
            header.info.height = h;
            header.info.mosaic = Mosaic::Bayer;
            header.info.bayer_pattern = Some(BayerPattern::GRBG);
            let raw = RawFrame {
                header,
                pixels: vec![1000.0_f32; w * h],
            };
            let ov = fast_cfa_overview(&raw, 2048, &cancel).unwrap();
            assert!(ov.width > 0 && ov.width <= 2048);
            assert!(ov.height > 0 && ov.height <= 2048);
            assert_eq!(ov.pixels.len(), ov.width * ov.height);
            for p in &ov.pixels {
                assert!(
                    (p[0] - 1000.0).abs() < 1e-3
                        && (p[1] - 1000.0).abs() < 1e-3
                        && (p[2] - 1000.0).abs() < 1e-3,
                    "Grid artifact detected at camera size {w}x{h}: pixel = {p:?}"
                );
            }
        }
    }

    #[test]
    fn test_wcs_cards_roundtrip_and_atomic_save_fits_and_xisf() {
        use crate::sky::fits_wcs::cards;
        use crate::sky::solve::{Poly, Wcs};
        use fulgur_core::fits::save_wcs_to_file;

        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);

        let wcs = Wcs {
            ra0: 83.8221,
            dec0: -5.3911,
            centre: [539.5, 959.5],
            pixel_norm: 960.0,
            sky_norm: 960.0 * 2.388,
            forward: Poly {
                degree: 2,
                x: vec![0.0, -0.998, 0.015, 0.0012, -0.0005, 0.0008],
                y: vec![0.0, -0.015, -0.998, -0.0004, 0.0009, -0.0006],
            },
            inverse: Poly {
                degree: 1,
                x: vec![0.0, -0.998, -0.015],
                y: vec![0.0, 0.015, -0.998],
            },
            width: 1080,
            height: 1920,
        };

        let wcs_cards: Vec<(String, String, String)> = cards(&wcs, [0.0, 0.0])
            .unwrap()
            .into_iter()
            .map(|(k, v, c)| (k, v, c.to_owned()))
            .collect();

        // 1. Atomic WCS save to FITS file preserves pixels bit-for-bit and populates header_wcs
        let fits_path = dir.path().join("atomic_wcs.fits");
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "32"),
            ("NAXIS2", "32"),
            ("BZERO", "32768"),
            ("BSCALE", "1"),
        ]);
        let mut data = Vec::with_capacity(32 * 32 * 2);
        for i in 0..(32 * 32) {
            let adu: u16 = 1234 + (i as u16 * 17);
            let stored = (adu as i32 - 32768) as i16;
            data.extend_from_slice(&stored.to_be_bytes());
        }
        bytes.extend_from_slice(&pad_data_block(data));
        std::fs::write(&fits_path, bytes).unwrap();

        let ov_before = load_frame_overview(&fits_path, 0, LayoutChoice::Mono, &cancel).unwrap();
        assert!(ov_before.header_wcs.is_none());

        save_wcs_to_file(&fits_path, 0, &wcs_cards).unwrap();

        let ov_after = load_frame_overview(&fits_path, 0, LayoutChoice::Mono, &cancel).unwrap();
        assert_eq!(ov_before.pixels, ov_after.pixels);
        let parsed_rep = ov_after
            .header_wcs
            .as_ref()
            .expect("FITS header_wcs should be parsed");
        assert!((parsed_rep.wcs.ra0 - wcs.ra0).abs() < 1e-5);
        assert!((parsed_rep.wcs.dec0 - wcs.dec0).abs() < 1e-5);
        assert_eq!(parsed_rep.wcs.forward.degree, 2);

        // 2. Atomic WCS save to XISF file preserves pixels bit-for-bit and populates header_wcs
        let xisf_path = dir.path().join("atomic_wcs.xisf");
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<xisf version="1.0">
  <Image geometry="16:16:1" sampleFormat="Float32" colorSpace="Gray" location="attachment:4096:1024">
    <FITSKeyword name="OBJECT" value="'M 42'" comment="Orion Nebula"/>
  </Image>
</xisf>"#;
        let xml_bytes = xml.as_bytes();
        let mut xisf_bytes = Vec::new();
        xisf_bytes.extend_from_slice(b"XISF0100");
        xisf_bytes.extend_from_slice(&(xml_bytes.len() as u32).to_le_bytes());
        xisf_bytes.extend_from_slice(&0u32.to_le_bytes());
        xisf_bytes.extend_from_slice(xml_bytes);
        xisf_bytes.resize(4096, 0);
        for i in 0..256 {
            xisf_bytes.extend_from_slice(&((i as f32) * 3.25 + 100.0).to_le_bytes());
        }
        std::fs::write(&xisf_path, xisf_bytes).unwrap();

        let xisf_before = load_frame_overview(&xisf_path, 0, LayoutChoice::Mono, &cancel).unwrap();
        assert!(xisf_before.header_wcs.is_none());

        save_wcs_to_file(&xisf_path, 0, &wcs_cards).unwrap();

        let xisf_after = load_frame_overview(&xisf_path, 0, LayoutChoice::Mono, &cancel).unwrap();
        assert_eq!(xisf_before.pixels, xisf_after.pixels);
        let parsed_xisf = xisf_after
            .header_wcs
            .as_ref()
            .expect("XISF header_wcs should be parsed");
        assert!((parsed_xisf.wcs.ra0 - wcs.ra0).abs() < 1e-5);
        assert!((parsed_xisf.wcs.dec0 - wcs.dec0).abs() < 1e-5);
        assert_eq!(parsed_xisf.wcs.forward.degree, 2);
    }

    fn sample_wcs_cards() -> Vec<(String, String, String)> {
        use crate::sky::fits_wcs::cards;
        use crate::sky::solve::{Poly, Wcs};
        let wcs = Wcs {
            ra0: 83.8221,
            dec0: -5.3911,
            centre: [15.5, 15.5],
            pixel_norm: 16.0,
            sky_norm: 16.0 * 2.388,
            forward: Poly {
                degree: 1,
                x: vec![0.0, -0.998, 0.015],
                y: vec![0.0, -0.015, -0.998],
            },
            inverse: Poly {
                degree: 1,
                x: vec![0.0, -0.998, -0.015],
                y: vec![0.0, 0.015, -0.998],
            },
            width: 32,
            height: 32,
        };
        cards(&wcs, [0.0, 0.0])
            .unwrap()
            .into_iter()
            .map(|(k, v, c)| (k, v, c.to_owned()))
            .collect()
    }

    fn u16_plane(n: usize, base: u16) -> Vec<u8> {
        let mut data = Vec::with_capacity(n * 2);
        for i in 0..n {
            let adu = base.wrapping_add((i % 977) as u16);
            data.extend_from_slice(&((adu as i32 - 32768) as i16).to_be_bytes());
        }
        pad_data_block(data)
    }

    fn leftover_tmp_files(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".fulgur-tmp-") || n.ends_with(".tmp"))
            .collect()
    }

    /// Finding 05: an unknown HDU / image index must be an error that leaves the file
    /// byte-identical, never a silent write into a different HDU.
    #[test]
    fn test_wcs_save_rejects_unknown_hdu_and_leaves_file_untouched() {
        use fulgur_core::fits::save_wcs_to_file;
        let dir = tempfile::tempdir().unwrap();
        let wcs_cards = sample_wcs_cards();

        let fits_path = dir.path().join("single.fits");
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "32"),
            ("NAXIS2", "32"),
            ("BZERO", "32768"),
        ]);
        bytes.extend_from_slice(&u16_plane(32 * 32, 1000));
        std::fs::write(&fits_path, &bytes).unwrap();
        assert!(save_wcs_to_file(&fits_path, 5, &wcs_cards).is_err());
        assert_eq!(std::fs::read(&fits_path).unwrap(), bytes);

        let xisf_path = dir.path().join("single.xisf");
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<xisf version="1.0">
  <Image geometry="16:16:1" sampleFormat="Float32" colorSpace="Gray" location="attachment:4096:1024">
  </Image>
</xisf>"#;
        let mut xisf_bytes = Vec::new();
        xisf_bytes.extend_from_slice(b"XISF0100");
        xisf_bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        xisf_bytes.extend_from_slice(&0u32.to_le_bytes());
        xisf_bytes.extend_from_slice(xml.as_bytes());
        xisf_bytes.resize(4096, 0);
        for i in 0..256 {
            xisf_bytes.extend_from_slice(&((i as f32) + 100.0).to_le_bytes());
        }
        std::fs::write(&xisf_path, &xisf_bytes).unwrap();
        assert!(save_wcs_to_file(&xisf_path, 3, &wcs_cards).is_err());
        assert_eq!(std::fs::read(&xisf_path).unwrap(), xisf_bytes);

        assert!(leftover_tmp_files(dir.path()).is_empty());
    }

    /// Finding 05: in a multi-extension FITS the WCS lands exactly in the selected HDU,
    /// and the pixel data of every HDU stays bit-identical.
    #[test]
    fn test_wcs_save_mef_targets_selected_extension_only() {
        use fulgur_core::fits::{read_header_hdu, read_raw_hdu, save_wcs_to_file};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mef.fits");
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "32"),
            ("NAXIS2", "32"),
            ("BZERO", "32768"),
            ("EXTEND", "T"),
        ]);
        bytes.extend_from_slice(&u16_plane(32 * 32, 1000));
        bytes.extend_from_slice(&write_header_block(&[
            ("XTENSION", "'IMAGE   '"),
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "32"),
            ("NAXIS2", "32"),
            ("PCOUNT", "0"),
            ("GCOUNT", "1"),
            ("BZERO", "32768"),
        ]));
        bytes.extend_from_slice(&u16_plane(32 * 32, 5000));
        std::fs::write(&path, &bytes).unwrap();

        let before0 = read_raw_hdu(&path, 0).unwrap().pixels;
        let before1 = read_raw_hdu(&path, 1).unwrap().pixels;

        save_wcs_to_file(&path, 1, &sample_wcs_cards()).unwrap();

        assert!(
            read_header_hdu(&path, 0)
                .unwrap()
                .card_f64("CRVAL1")
                .is_none()
        );
        assert!(
            read_header_hdu(&path, 1)
                .unwrap()
                .card_f64("CRVAL1")
                .is_some()
        );
        assert_eq!(read_raw_hdu(&path, 0).unwrap().pixels, before0);
        assert_eq!(read_raw_hdu(&path, 1).unwrap().pixels, before1);
        assert!(leftover_tmp_files(dir.path()).is_empty());
    }

    fn write_float_fits(
        path: &std::path::Path,
        w: usize,
        h: usize,
        value: impl Fn(usize, usize) -> f32,
    ) {
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "-32"),
            ("NAXIS", "2"),
            ("NAXIS1", &w.to_string()),
            ("NAXIS2", &h.to_string()),
        ]);
        let mut data = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&value(x, y).to_be_bytes());
            }
        }
        bytes.extend_from_slice(&pad_data_block(data));
        std::fs::write(path, bytes).unwrap();
    }

    /// Finding 04: the 1:1 ROI reader must use the same float scaling as the full frame.
    /// Before the fix, a dark ROI (all values <= 2) of an ADU-valued image was scaled 65535x.
    #[test]
    fn test_float_fits_roi_uses_same_scale_as_full_frame() {
        use fulgur_core::fits::{read_header_hdu, read_raw_from_header, read_roi};
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);

        // ADU-valued float image with a dark corner (values 0.5 .. 1.5).
        let adu_path = dir.path().join("adu_float.fits");
        write_float_fits(&adu_path, 64, 64, |x, y| {
            if x < 16 && y < 16 {
                0.5 + (x as f32) / 16.0
            } else {
                1000.0 + (x + y) as f32
            }
        });
        let header = read_header_hdu(&adu_path, 0).unwrap();
        let full = read_raw_from_header(&adu_path, header.clone()).unwrap();
        let roi = read_roi(&adu_path, &header, [0, 0, 8, 8], &cancel).unwrap();
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(roi.pixels[y * 8 + x], full.pixels[y * 64 + x]);
            }
        }
        assert!(full.pixels[0] < 2.0, "ADU image must not be rescaled");

        // Normalized 0..1 master: both readers scale to 0..65535.
        let norm_path = dir.path().join("normalized_float.fits");
        write_float_fits(&norm_path, 64, 64, |x, y| ((x + y) as f32) / 128.0);
        let header = read_header_hdu(&norm_path, 0).unwrap();
        let full = read_raw_from_header(&norm_path, header.clone()).unwrap();
        let roi = read_roi(&norm_path, &header, [32, 32, 8, 8], &cancel).unwrap();
        assert!((full.pixels[64 * 63 + 63] - 126.0 / 128.0 * 65535.0).abs() < 1.0);
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(roi.pixels[y * 8 + x], full.pixels[(32 + y) * 64 + 32 + x]);
            }
        }
    }

    /// ADR-022: 8-bit processed exports get no FWHM / HFR / tilt (only channel levels),
    /// while the same star field saved as 16-bit is still measured.
    #[test]
    fn test_star_metrics_withheld_for_8bit_exports_only() {
        use crate::stars::{RENDERED_IMAGE_STARS_MESSAGE, analyze_frame_stars};

        let (w, h) = (256_u32, 192_u32);
        let value = |x: u32, y: u32| -> f64 {
            let mut v = 0.08 + f64::from((x * 7 + y * 13) % 11) * 0.002;
            for sy in (24..h - 24).step_by(36) {
                for sx in (24..w - 24).step_by(36) {
                    let d2 = f64::from(x.abs_diff(sx).pow(2) + y.abs_diff(sy).pow(2));
                    v += 0.6 * (-d2 / (2.0 * 1.6 * 1.6)).exp();
                }
            }
            v.min(1.0)
        };
        let dir = tempfile::tempdir().unwrap();
        let png8 = dir.path().join("export_8bit.png");
        image::GrayImage::from_fn(w, h, |x, y| image::Luma([(value(x, y) * 255.0) as u8]))
            .save(&png8)
            .unwrap();
        let png16 = dir.path().join("linear_16bit.png");
        image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::from_fn(w, h, |x, y| {
            image::Luma([(value(x, y) * 65535.0) as u16])
        })
        .save(&png16)
        .unwrap();

        let cancel = AtomicBool::new(false);
        let r8 = analyze_frame_stars(&png8, 0, LayoutChoice::Auto, &cancel).unwrap();
        assert_eq!(r8.metrics_unavailable, Some(RENDERED_IMAGE_STARS_MESSAGE));
        assert!(r8.stars.is_empty() && r8.median_fwhm_px.is_none());
        assert_eq!(r8.levels.len(), 4, "channel levels stay available");

        let r16 = analyze_frame_stars(&png16, 0, LayoutChoice::Auto, &cancel).unwrap();
        assert!(r16.metrics_unavailable.is_none());
        assert!(r16.star_count > 0 && r16.median_fwhm_px.is_some());
    }

    /// Finding 02: a RAW whose sensor data cannot be decoded (here a CR3 container holding
    /// only a JPEG) is labelled as preview-only and star metrics are refused.
    #[test]
    fn test_raw_without_decodable_sensor_data_is_preview_only() {
        use crate::stars::{RAW_PREVIEW_ONLY_STARS_MESSAGE, analyze_frame_stars};
        use crate::stretch::load_raw_stage2_overview;
        use fulgur_core::raw::RawStage;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("IMG_0001.CR3");
        let mut jpeg = Vec::new();
        // Busy pattern so the JPEG exceeds the 4 KiB minimum of the embedded-preview scan.
        let preview = image::RgbImage::from_fn(512, 384, |x, y| {
            image::Rgb([((x * 7) ^ (y * 13)) as u8, ((x * y) % 251) as u8, 80])
        });
        image::DynamicImage::ImageRgb8(preview)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let mut bytes = vec![0, 0, 0, 24];
        bytes.extend_from_slice(b"ftypcrx ");
        bytes.resize(64, 0);
        bytes.extend_from_slice(&jpeg);
        bytes.resize(bytes.len() + 64, 0);
        std::fs::write(&path, bytes).unwrap();

        let cancel = AtomicBool::new(false);
        let frame = load_raw_stage2_overview(&path, LayoutChoice::Auto, &cancel).unwrap();
        assert_eq!(frame.raw_stage, RawStage::Stage2PreviewOnly);

        let err = analyze_frame_stars(&path, 0, LayoutChoice::Auto, &cancel).unwrap_err();
        assert_eq!(err.to_string(), RAW_PREVIEW_ONLY_STARS_MESSAGE);
    }

    /// Finding 07: header geometry beyond the decode budget is rejected with an error
    /// instead of aborting the process on allocation.
    #[test]
    fn test_absurd_geometry_is_rejected_not_allocated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.fits");
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "-32"),
            ("NAXIS", "2"),
            ("NAXIS1", "200000"),
            ("NAXIS2", "200000"),
        ]);
        bytes.extend_from_slice(&pad_data_block(vec![0; 64]));
        std::fs::write(&path, bytes).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(load_frame_overview(&path, 0, LayoutChoice::Mono, &cancel).is_err());
        assert!(fulgur_core::try_alloc_zeroed::<u8>(fulgur_core::MAX_DECODE_BYTES + 1).is_err());
    }

    /// Finding 06: saving WCS into an XISF removes PixInsight's stale
    /// `PCL:AstrometricSolution:*` properties (PixInsight prefers them over FITS keywords),
    /// keeps other properties, and correctly shifts single-quoted attachment offsets when the
    /// header grows past the first attachment.
    #[test]
    fn test_xisf_wcs_save_drops_pcl_solution_and_shifts_single_quoted_attachment() {
        use fulgur_core::fits::save_wcs_to_file;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi_solved.xisf");
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<xisf version="1.0">
  <Image geometry='16:16:1' sampleFormat='Float32' colorSpace='Gray' location='attachment:1024:1024'>
    <Property id="PCL:AstrometricSolution:ReferenceCelestialCoordinates" type="F64Vector" length="2">10 20</Property>
    <Property id="PCL:AstrometricSolution:ProjectionSystem" type="String" value="Gnomonic"/>
    <Property id="Observation:Object:Name" type="String" value="M 42"/>
  </Image>
</xisf>"#;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"XISF0100");
        bytes.extend_from_slice(&(xml.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(xml.as_bytes());
        bytes.resize(1024, 0);
        for i in 0..256 {
            bytes.extend_from_slice(&((i as f32) * 2.0 + 50.0).to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();

        let cancel = AtomicBool::new(false);
        let before = load_frame_overview(&path, 0, LayoutChoice::Mono, &cancel).unwrap();
        save_wcs_to_file(&path, 0, &sample_wcs_cards()).unwrap();
        let after = load_frame_overview(&path, 0, LayoutChoice::Mono, &cancel).unwrap();
        assert_eq!(before.pixels, after.pixels);
        assert!(after.header_wcs.is_some());

        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        assert!(!text.contains("PCL:AstrometricSolution"));
        assert!(text.contains("Observation:Object:Name"));
        assert!(
            !text.contains("attachment:1024:"),
            "attachment offset must be shifted"
        );
    }

    /// Finding 08: star metrics computed from the plane decoded for the overview are
    /// identical to metrics from a fresh read of the file.
    #[test]
    fn test_reused_source_gives_identical_star_metrics() {
        use crate::stars::{analyze_frame_stars, analyze_raw_frame};
        use crate::stretch::load_frame_overview_with_source;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stars.fits");
        let (w, h) = (640_usize, 480_usize);
        let mut img = vec![1000.0_f64; w * h];
        for (i, v) in img.iter_mut().enumerate() {
            *v += ((i * 7919) % 7) as f64 - 3.0;
        }
        for sy in 0..5 {
            for sx in 0..6 {
                let (cx, cy) = (60.0 + sx as f64 * 100.0, 50.0 + sy as f64 * 95.0);
                for y in (cy as usize - 10)..(cy as usize + 10) {
                    for x in (cx as usize - 10)..(cx as usize + 10) {
                        let r2 = (x as f64 - cx).powi(2) + (y as f64 - cy).powi(2);
                        img[y * w + x] += 8000.0 * (-r2 / (2.0 * 1.8 * 1.8)).exp();
                    }
                }
            }
        }
        let mut bytes = write_header_block(&[
            ("SIMPLE", "T"),
            ("BITPIX", "16"),
            ("NAXIS", "2"),
            ("NAXIS1", "640"),
            ("NAXIS2", "480"),
            ("BZERO", "32768"),
        ]);
        let mut data = Vec::with_capacity(w * h * 2);
        for v in &img {
            let adu = v.round().clamp(0.0, 65535.0) as i32;
            data.extend_from_slice(&((adu - 32768) as i16).to_be_bytes());
        }
        bytes.extend_from_slice(&pad_data_block(data));
        std::fs::write(&path, bytes).unwrap();

        let cancel = AtomicBool::new(false);
        let (_, source) =
            load_frame_overview_with_source(&path, 0, LayoutChoice::Mono, &cancel).unwrap();
        let source = source.expect("mono FITS keeps its decoded plane");
        let reused = analyze_raw_frame(&source, &cancel).unwrap();
        let fresh = analyze_frame_stars(&path, 0, LayoutChoice::Mono, &cancel).unwrap();
        assert!(reused.star_count > 10);
        assert_eq!(reused.star_count, fresh.star_count);
        assert_eq!(reused.median_fwhm_px, fresh.median_fwhm_px);
        assert_eq!(reused.median_hfr_px, fresh.median_hfr_px);
    }
}
