//! tests for debug.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    #[test]
    fn structural_noise_is_filtered_by_exact_role() {
        for role in ["none", "presentation", "generic", "ScrollBar"] {
            assert!(is_structural_noise(role), "{role} should be noise");
        }
        // Substring matching would swallow real content: "GenericButton" and
        // "presentationCard" are not the roles being filtered.
        for role in ["Button", "StaticLabel", "ScrollableRegion", "Generic"] {
            assert!(!is_structural_noise(role), "{role} must survive");
        }
    }

    #[test]
    fn standalone_text_is_listed_but_a_controls_own_label_is_not() {
        // A validation message is the only thing on screen explaining why a
        // button did nothing, so it has to survive as text of its own.
        assert!(is_listable_text(
            "StaticText",
            "at least one model is required",
            false
        ));
        // The same words inside a button are already carried by the button's
        // name; listing them again just buries everything else.
        assert!(!is_listable_text("StaticText", "Save", true));
        // Whitespace is not a message.
        assert!(!is_listable_text("StaticText", "   ", false));
        // Only text is judged here; other roles are decided by the noise filter.
        assert!(!is_listable_text("Button", "Save", false));
    }

    #[test]
    fn near_misses_rank_the_field_the_caller_meant_first() {
        // A form names its rows with an index, so the name a caller reaches
        // for is the label without it.
        let tree = json!({
            "nodes": [
                { "role": { "value": "textbox" }, "name": { "value": "Model ID 1" } },
                { "role": { "value": "button" }, "name": { "value": "Model" } },
                { "role": { "value": "textbox" }, "name": { "value": "Provider ID" } },
            ]
        });
        let near = near_miss_names(&tree, "Model ID", 8);
        assert_eq!(
            near.first().map(String::as_str),
            Some("textbox \"Model ID 1\""),
            "{near:?}"
        );
        // Unrelated names stay out of the answer entirely.
        assert!(
            !near.iter().any(|entry| entry.contains("Provider ID")),
            "{near:?}"
        );
    }

    #[test]
    fn near_misses_are_empty_when_nothing_is_close() {
        let tree = json!({
            "nodes": [
                { "role": { "value": "button" }, "name": { "value": "Save" } },
            ]
        });
        assert!(near_miss_names(&tree, "zzz", 8).is_empty());
    }

    #[test]
    fn clickable_roles_match_chromes_canonical_casing() {
        for role in ["button", "link", "textbox", "checkbox", "combobox", "tab"] {
            assert!(is_clickable_role(role), "{role} should be clickable");
        }
        // Chrome emits "Button"; a lowercase compare would report every
        // button on the page as unclickable.
        assert!(
            !is_clickable_role("Button"),
            "canonical case is not matched"
        );
        for role in ["heading", "paragraph", "image", "list"] {
            assert!(!is_clickable_role(role), "{role} must not be clickable");
        }
    }

    #[test]
    fn key_spec_resolves_aliases_to_one_canonical_spelling() {
        for alias in ["Enter", "submit", "return", "  Enter  "] {
            let spec = key_spec(alias.trim()).expect("Enter should resolve");
            assert_eq!(spec.key, "Enter");
        }
        assert_eq!(key_spec("esc").map(|spec| spec.key), Some("Escape"));
        assert_eq!(key_spec("up").map(|spec| spec.key), Some("ArrowUp"));
        // Submitting is Enter's only textual payload; a modifier that carried
        // one would insert a control character into the field.
        assert_eq!(key_spec("Enter").and_then(|spec| spec.text), Some("\r"));
        assert_eq!(key_spec("Shift").and_then(|spec| spec.text), None);
    }

    #[test]
    fn key_spec_refuses_names_it_cannot_honour() {
        // A guessed key code is worse than no key: it dispatches an event the
        // page did not ask for and reports success.
        for name in ["F13", "nope", "", "Ctrl+S"] {
            assert!(key_spec(name).is_none(), "{name:?} must not resolve");
        }
    }

    #[test]
    fn quad_bounds_reads_the_flat_number_run_cdp_actually_returns() {
        // Captured from headless Edge, content quad of a button at viewport
        // y=200 on a page scrolled to 1000. Read as `{x, y}` points this
        // yields eight zeroes and every element reports as 0x0 at the origin.
        let quad = [48.0, 203.0, 232.0, 203.0, 232.0, 237.0, 48.0, 237.0];
        let (x, y, width, height) = quad_bounds(&quad).expect("quad should parse");
        assert_eq!((x, y), (48.0, 203.0));
        assert_eq!((width, height), (184.0, 34.0));
    }

    #[test]
    fn quad_bounds_survives_unordered_and_degenerate_input() {
        // Vertices are not guaranteed to arrive corner-first.
        let (x, y, width, height) = quad_bounds(&[232.0, 237.0, 48.0, 203.0, 232.0, 203.0])
            .expect("three points still bound a box");
        assert_eq!((x, y), (48.0, 203.0));
        assert_eq!((width, height), (184.0, 34.0));
        // A single point is a point, not a box; returning None keeps the
        // caller from reporting a plausible-looking zero size.
        assert_eq!(quad_bounds(&[10.0, 10.0]), None);
        assert_eq!(quad_bounds(&[]), None);
        // A non-finite coordinate would poison the min/max and poison every
        // click derived from it.
        assert_eq!(
            quad_bounds(&[f64::NAN, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0]),
            None
        );
    }
    #[test]
    fn hit_test_js_never_splices_a_selector_into_script() {
        // A selector is untrusted input. Pasted raw it would turn a hit test
        // into arbitrary script execution inside the debugged page.
        let expression = hit_test_js(1.0, 2.0, Some("img[src='x']; alert(1)"));
        // The selector travels as a JSON string literal, so its semicolon is
        // inside quotes and cannot terminate the statement it sits in.
        assert!(expression.contains(r#""img[src='x']; alert(1)""#));
        // And nothing from the input became executable script.
        assert!(!expression.contains("= alert(1)"));
        // A click with no intended element must not smuggle a selector in.
        assert!(!hit_test_js(1.0, 2.0, None).contains("querySelector(\\\""));
    }

    #[test]
    fn png_dimensions_read_the_header_chrome_does_not_send() {
        // Page.captureScreenshot answers with base64 and no size, so the size
        // has to come out of the file: the eight-byte signature, the IHDR
        // chunk length, "IHDR", then the two dimensions as big-endian u32.
        let one_by_one: Vec<u8> = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        ]
        .to_vec();
        assert_eq!(png_dimensions(&one_by_one), Some((1, 1)));

        let wide: Vec<u8> = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
            b'D', b'R', 0x00, 0x00, 0x07, 0x80, 0x00, 0x00, 0x04, 0x38,
        ]
        .to_vec();
        assert_eq!(png_dimensions(&wide), Some((1920, 1080)));

        // A file that is not a PNG must be refused rather than measured as
        // zero: a reply claiming 0x0 describes a capture nobody wants, and the
        // caller would have no way to tell it from a blank page.
        assert_eq!(png_dimensions(b"not a png at all, but long enough"), None);
        // A truncated header is the same answer.
        assert_eq!(png_dimensions(&[0x89, 0x50, 0x4E, 0x47]), None);
    }

    #[test]
    fn prune_screenshots_keeps_the_newest_and_leaves_what_is_not_a_png() {
        let dir = std::env::temp_dir().join(format!(
            "dshbox-prune-{}-{}",
            std::process::id(),
            "png_dimensions_read_the_header_chrome_does_not_send"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        // Written oldest first, so which files survive is decided by mtime
        // rather than by name.
        for index in 0..5 {
            std::fs::write(dir.join(format!("{index}.png")), b"x").expect("write");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::write(dir.join("notes.txt"), b"keep me").expect("write");

        prune_screenshots(&dir, 2);

        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        // The two newest survive, and something that is not a PNG is none of
        // this function's business.
        assert_eq!(
            left,
            vec![
                "3.png".to_owned(),
                "4.png".to_owned(),
                "notes.txt".to_owned()
            ]
        );

        // Fewer files than the cap is a no-op, not a reason to delete anything.
        prune_screenshots(&dir, 50);
        assert_eq!(std::fs::read_dir(&dir).expect("read dir").count(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_screenshot_turns_what_chrome_sent_into_a_measured_file() {
        // A real 4x3 PNG, base64 as Page.captureScreenshot returns it. Using a
        // genuine image rather than a hand-built header is the point: the whole
        // path under test is decode -> measure -> write, and a header-only
        // fixture would prove none of the decoding.
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAQAAAADCAYAAAC09K7GAAAAEklEQVR4nGMQMgn7j4wZCAoAANoSE0XZSPwFAAAAAElFTkSuQmCC";
        let dir = std::env::temp_dir().join(format!(
            "dshbox-shot-{}-{}",
            std::process::id(),
            "write_screenshot_turns_what_chrome_sent_into_a_measured_file"
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let shot = write_screenshot(&dir, encoded).expect("a real PNG should be written");

        assert_eq!((shot.width, shot.height), (4, 3));
        assert_eq!(shot.bytes, 75);
        assert!(shot.path.starts_with(&dir), "{:?}", shot.path);
        assert_eq!(
            shot.path.extension().and_then(|ext| ext.to_str()),
            Some("png")
        );
        // The file is the image, not a placeholder: reading it back decodes to
        // the same bytes Chrome sent.
        let written = std::fs::read(&shot.path).expect("read back");
        assert_eq!(written.len(), 75);
        assert_eq!(png_dimensions(&written), Some((4, 3)));

        // Something that is not base64 of a PNG is refused, and refuses
        // loudly: the caller is told the capture failed rather than handed a
        // path to a file that is not what was asked for.
        assert!(write_screenshot(&dir, "not base64 at all!!").is_err());
        let jpeg_like = base64::engine::general_purpose::STANDARD.encode([
            0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let error = write_screenshot(&dir, &jpeg_like).expect_err("a JPEG must be refused");
        assert!(error.contains("not a PNG"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }
