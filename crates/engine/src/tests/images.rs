//! Image sources (ADR 0019): files in the project directory, their limits,
//! reading them again, and projects whose image is missing.

use command_api::{ImageFileList, SourceKind, SourceRect};

use super::*;
use crate::images::ImageStore;

/// Write a `width` x `height` PNG of one opaque `rgb` colour.
fn write_png(dir: &std::path::Path, name: &str, width: u32, height: u32, rgb: [u8; 3]) {
    let file = std::fs::File::create(dir.join(name)).expect("create");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("header");
    let pixels: Vec<u8> = (0..width * height).flat_map(|_| rgb).collect();
    writer.write_image_data(&pixels).expect("pixels");
}

fn images(engine: &mut Engine) -> ImageFileList {
    match run(engine, "list_images", json!({})) {
        CommandOutput::ImageFiles(list) => list,
        other => panic!("unexpected output: {other:?}"),
    }
}

fn add_image(engine: &mut Engine, name: &str, file: &str) -> Result<CommandOutput, CommandError> {
    try_run(
        engine,
        "add_source",
        json!({"scene_name": "Show", "name": name, "kind": "image", "file": file}),
    )
}

fn source_rect(engine: &mut Engine, name: &str) -> SourceRect {
    let list = scene_list(engine);
    let scene = list.scenes.iter().find(|scene| scene.name == "Show");
    let source = scene
        .and_then(|scene| scene.sources.iter().find(|source| source.name == name))
        .unwrap_or_else(|| panic!("no source `{name}`"));
    source.rect
}

#[test]
fn the_project_directory_lists_images_with_their_size() {
    let dir = TempDir::new("list-images");
    write_png(&dir.0, "logo.png", 320, 200, [255, 0, 0]);
    write_png(&dir.0, "b.PNG", 2, 1, [0, 0, 0]);
    std::fs::write(dir.0.join("broken.jpg"), b"not a jpeg").expect("write");
    std::fs::write(dir.0.join("notes.txt"), b"text").expect("write");
    std::fs::write(dir.0.join("show.json"), b"{}").expect("write");
    std::fs::create_dir(dir.0.join("folder.png")).expect("dir");
    let mut engine = engine().with_project_dir(&dir.0);
    let list = images(&mut engine);
    let listed: Vec<_> = list
        .files
        .iter()
        .map(|file| (file.file_name.as_str(), file.width, file.height))
        .collect();
    assert_eq!(
        listed,
        [
            ("b.PNG", Some(2), Some(1)),
            ("broken.jpg", None, None),
            ("logo.png", Some(320), Some(200)),
        ]
    );
    assert_eq!(list.omitted, 0);
    assert_eq!(list.files[1].bytes, 10);
    assert!(list.files[2].modified_unix_ms.is_some());

    // Without a project directory there is nothing to list or show.
    let mut nowhere = self::engine();
    run(&mut nowhere, "create_scene", json!({"name": "Show"}));
    for error in [
        try_run(&mut nowhere, "list_images", json!({})).unwrap_err(),
        add_image(&mut nowhere, "logo", "logo.png").unwrap_err(),
    ] {
        assert_eq!(error.code, ErrorCode::InvalidState, "{error}");
        assert!(error.message.contains("--project-dir"), "{error}");
    }
}

#[test]
fn an_image_keeps_its_own_size_and_bad_files_are_refused() {
    let dir = TempDir::new("add-image");
    write_png(&dir.0, "logo.png", 320, 200, [255, 0, 0]);
    write_png(&dir.0, "wide.png", 3840, 1080, [0, 255, 0]);
    write_png(&dir.0, "huge.png", 8193, 1, [0, 0, 255]);
    std::fs::write(dir.0.join("broken.png"), b"\x89PNG\r\n\x1a\nbroken").expect("write");
    let mut engine = engine().with_project_dir(&dir.0);
    run(&mut engine, "create_scene", json!({"name": "Show"}));

    add_image(&mut engine, "logo", "logo.png").expect("add");
    let list = scene_list(&mut engine);
    let logo = &list.scenes[1].sources[0];
    assert_eq!(
        logo.kind,
        SourceKind::Image {
            file: "logo.png".to_owned()
        }
    );
    assert_eq!(
        logo.rect,
        SourceRect {
            x: 0.0,
            y: 0.0,
            width: 320.0,
            height: 200.0
        }
    );
    // Larger than the canvas: scaled down to it, aspect kept.
    add_image(&mut engine, "wide", "wide.png").expect("add");
    assert_eq!(
        source_rect(&mut engine, "wide"),
        SourceRect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 540.0
        }
    );
    // A side scaled below a pixel stays a pixel.
    write_png(&dir.0, "tall.png", 1, 4000, [0, 0, 255]);
    add_image(&mut engine, "tall", "tall.png").expect("add");
    assert_eq!(source_rect(&mut engine, "tall").width, 1.0);
    // A rectangle given is kept; the image is fitted into it when drawn.
    run(
        &mut engine,
        "add_source",
        json!({"scene_name": "Show", "name": "boxed", "kind": "image", "file": "logo.png",
               "x": 10, "y": 20, "width": 100, "height": 100}),
    );
    assert_eq!(source_rect(&mut engine, "boxed").width, 100.0);

    let missing = add_image(&mut engine, "gone", "gone.png").unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound, "{missing}");
    assert_eq!(missing.details, Some(json!({"file": "gone.png"})));
    let broken = add_image(&mut engine, "broken", "broken.png").unwrap_err();
    assert_eq!(broken.code, ErrorCode::InvalidParams, "{broken}");
    let huge = add_image(&mut engine, "huge", "huge.png").unwrap_err();
    assert_eq!(huge.code, ErrorCode::InvalidParams, "{huge}");
    let details = huge.details.expect("details");
    assert_eq!(
        (details["width"].as_u64(), details["max_side"].as_u64()),
        (Some(8193), Some(8192))
    );
    // Refused adds leave nothing behind.
    assert_eq!(scene_list(&mut engine).scenes[1].sources.len(), 4);
    assert!(engine.images.frame("broken.png").is_none());
    assert!(engine.images.frame("huge.png").is_none());
}

#[test]
fn image_frames_are_shared_bounded_and_dropped_with_their_sources() {
    let dir = TempDir::new("image-memory");
    // 100 x 100 x 4 bytes each; two fit, three do not.
    write_png(&dir.0, "a.png", 100, 100, [255, 0, 0]);
    write_png(&dir.0, "b.png", 100, 100, [0, 255, 0]);
    write_png(&dir.0, "c.png", 100, 100, [0, 0, 255]);
    let mut engine = engine().with_project_dir(&dir.0);
    engine.images = ImageStore::with_max_total_bytes(2 * 100 * 100 * 4);
    run(&mut engine, "create_scene", json!({"name": "Show"}));
    add_image(&mut engine, "a1", "a.png").expect("add");
    let frame = Arc::downgrade(engine.images.frame("a.png").expect("frame"));
    // The same file again is the same frame, counted once.
    add_image(&mut engine, "a2", "a.png").expect("add");
    let shared = engine.images.frame("a.png").expect("frame");
    assert!(Arc::ptr_eq(&frame.upgrade().expect("alive"), shared));
    add_image(&mut engine, "b", "b.png").expect("add");
    let full = add_image(&mut engine, "c", "c.png").unwrap_err();
    assert_eq!(full.code, ErrorCode::InvalidParams, "{full}");
    assert_eq!(full.details.expect("details")["bytes_used"], 80_000);
    // An image over its own limits says so, whatever the total.
    write_png(&dir.0, "huge.png", 8193, 1, [0, 0, 0]);
    let huge = add_image(&mut engine, "huge", "huge.png").unwrap_err();
    let details = huge.details.expect("details");
    assert_eq!(details["max_side"], 8192, "{}", huge.message);
    // Its frame goes with the last source showing it.
    run(
        &mut engine,
        "remove_source",
        json!({"scene_name": "Show", "source_name": "a1"}),
    );
    assert!(engine.images.frame("a.png").is_some());
    run(
        &mut engine,
        "remove_source",
        json!({"scene_name": "Show", "source_name": "a2"}),
    );
    assert!(engine.images.frame("a.png").is_none());
    assert!(frame.upgrade().is_none(), "the frame is freed");
    // A file read for an add that then fails (the name is taken) is not
    // kept.
    let duplicate = add_image(&mut engine, "b", "c.png").unwrap_err();
    assert!(duplicate.message.contains("already has"), "{duplicate}");
    assert!(engine.images.frame("c.png").is_none());
    add_image(&mut engine, "c", "c.png").expect("fits now");
}

#[test]
fn images_draw_live_and_are_read_again_on_restart() {
    let _media = media_test_lock();
    let dir = TempDir::new("image-live");
    write_png(&dir.0, "logo.png", 960, 540, [255, 0, 0]);
    write_png(&dir.0, "other.png", 960, 540, [0, 255, 0]);
    let mut engine = engine().with_project_dir(&dir.0);
    run(&mut engine, "create_scene", json!({"name": "Show"}));
    run(
        &mut engine,
        "set_active_scene",
        json!({"scene_name": "Show"}),
    );
    run(&mut engine, "set_preview", json!({"enabled": true}));
    add_image(&mut engine, "logo", "logo.png").expect("add");
    // The image covers the top left quarter (canvas pixels); the preview
    // is a quarter of the canvas.
    wait_for_preview_pixel(&engine, 10, 10, [0, 0, 255, 255]);
    wait_for_preview_pixel(&engine, 400, 200, [0, 0, 0, 255]);

    // The file changes: shown after restart_source reads it again.
    write_png(&dir.0, "logo.png", 960, 540, [0, 0, 255]);
    run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Show", "source_name": "logo"}),
    );
    wait_for_preview_pixel(&engine, 10, 10, [255, 0, 0, 255]);
    // Another file, through update_source.
    run(
        &mut engine,
        "update_source",
        json!({"scene_name": "Show", "source_name": "logo", "file": "other.png"}),
    );
    wait_for_preview_pixel(&engine, 10, 10, [0, 255, 0, 255]);
    assert!(engine.images.frame("logo.png").is_none());
    // The same file again reads it again (a refresh at control access);
    // the old frame is freed once the compositor has swapped it.
    let old = Arc::downgrade(engine.images.frame("other.png").expect("frame"));
    write_png(&dir.0, "other.png", 960, 540, [255, 255, 255]);
    run(
        &mut engine,
        "update_source",
        json!({"scene_name": "Show", "source_name": "logo", "file": "other.png"}),
    );
    wait_for_preview_pixel(&engine, 10, 10, [255, 255, 255, 255]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while old.upgrade().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(old.upgrade().is_none(), "the replaced frame is freed");
    // A missing file changes nothing.
    let missing = try_run(
        &mut engine,
        "update_source",
        json!({"scene_name": "Show", "source_name": "logo", "file": "gone.png"}),
    )
    .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound, "{missing}");
    std::fs::remove_file(dir.0.join("other.png")).expect("remove");
    let gone = try_run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Show", "source_name": "logo"}),
    )
    .unwrap_err();
    assert_eq!(gone.code, ErrorCode::NotFound, "{gone}");
    wait_for_preview_pixel(&engine, 10, 10, [255, 255, 255, 255]);
    run(&mut engine, "set_preview", json!({"enabled": false}));
}

#[test]
fn image_sources_take_only_their_own_fields() {
    let dir = TempDir::new("image-fields");
    write_png(&dir.0, "logo.png", 4, 4, [255, 0, 0]);
    let mut engine = engine().with_project_dir(&dir.0);
    run(&mut engine, "create_scene", json!({"name": "Show"}));
    add_image(&mut engine, "logo", "logo.png").expect("add");
    run(
        &mut engine,
        "add_source",
        json!({"scene_name": "Show", "name": "bars", "kind": "color_bars"}),
    );
    let not_image = try_run(
        &mut engine,
        "update_source",
        json!({"scene_name": "Show", "source_name": "bars", "file": "logo.png"}),
    )
    .unwrap_err();
    assert_eq!(not_image.code, ErrorCode::InvalidParams, "{not_image}");
    assert!(
        not_image.message.contains("not an image source"),
        "{not_image}"
    );
    let target = try_run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Show", "source_name": "logo", "capture_target_id": "fake:1"}),
    )
    .unwrap_err();
    assert_eq!(target.code, ErrorCode::InvalidParams, "{target}");
    let bars = try_run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Show", "source_name": "bars"}),
    )
    .unwrap_err();
    assert_eq!(bars.code, ErrorCode::InvalidParams, "{bars}");
    // Reading an image again needs no pipeline.
    run(
        &mut engine,
        "restart_source",
        json!({"scene_name": "Show", "source_name": "logo"}),
    );
}

#[test]
fn a_project_whose_image_is_missing_loads_with_a_warning() {
    let dir = TempDir::new("image-project");
    write_png(&dir.0, "logo.png", 64, 32, [255, 0, 0]);
    let mut engine = engine().with_project_dir(&dir.0);
    run(&mut engine, "create_scene", json!({"name": "Show"}));
    add_image(&mut engine, "logo", "logo.png").expect("add");
    run(
        &mut engine,
        "save_project",
        json!({"file_name": "show.json"}),
    );

    let mut fresh = self::engine().with_project_dir(&dir.0);
    let CommandOutput::ProjectLoaded(loaded) = run(
        &mut fresh,
        "load_project",
        json!({"file_name": "show.json"}),
    ) else {
        panic!("unexpected output");
    };
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert!(fresh.images.frame("logo.png").is_some());

    std::fs::remove_file(dir.0.join("logo.png")).expect("remove");
    let mut moved = self::engine().with_project_dir(&dir.0);
    let CommandOutput::ProjectLoaded(loaded) = run(
        &mut moved,
        "load_project",
        json!({"file_name": "show.json"}),
    ) else {
        panic!("unexpected output");
    };
    assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
    assert!(
        loaded.warnings[0].contains("`logo`") && loaded.warnings[0].contains("logo.png"),
        "{:?}",
        loaded.warnings
    );
    // The source loads and draws nothing.
    let scene = &loaded.scenes.scenes[1];
    assert_eq!(scene.sources[0].name, "logo");
    let update = crate::scenes::build(
        scene,
        &crate::scenes::LiveFrames {
            captures: &[],
            images: &moved.images,
        },
        &mut crate::scenes::FrameCache::default(),
    )
    .expect("build");
    assert!(update.scene.layers.is_empty());
    // In an engine that had read the file, the old frame goes too.
    let CommandOutput::ProjectLoaded(again) = run(
        &mut fresh,
        "load_project",
        json!({"file_name": "show.json"}),
    ) else {
        panic!("unexpected output");
    };
    assert_eq!(again.warnings.len(), 1, "{:?}", again.warnings);
    assert!(fresh.images.frame("logo.png").is_none());
    // Once the file is back, restart_source shows it.
    write_png(&dir.0, "logo.png", 64, 32, [255, 0, 0]);
    run(
        &mut moved,
        "restart_source",
        json!({"scene_name": "Show", "source_name": "logo"}),
    );
    assert!(moved.images.frame("logo.png").is_some());
}
