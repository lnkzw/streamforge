//! A virtual camera for trying camera sources without a webcam: a PipeWire
//! output stream with `media.class` `Video/Source` and `media.role`
//! `Camera`, 1280x720 YUY2 at 30 fps, red and blue halves with a white bar
//! sweeping across.
//!
//! ```text
//! cargo run -p capture-linux --example virtual_camera [node-name]
//! ```
//!
//! It is listed by `streamforge capture cameras` as `node-name` (default
//! `streamforge-virtual-camera`) until it is stopped with Ctrl-C. PipeWire's
//! own `videotestsrc` node is not used: PipeWire 1.6.10 aborts when one is
//! destroyed.

#[cfg(target_os = "linux")]
mod camera {
    use std::cell::Cell;
    use std::time::Duration;

    use pipewire as pw;
    use pw::spa::param::ParamType;
    use pw::spa::pod::{Object, Pod, Property, Value};

    const WIDTH: usize = 1280;
    const HEIGHT: usize = 720;
    /// (Y, U, V) in BT.601 limited range, the default for formats that do
    /// not say.
    const RED: [u8; 3] = [81, 90, 240];
    const BLUE: [u8; 3] = [41, 240, 110];
    const WHITE: [u8; 3] = [235, 128, 128];
    const BAR_WIDTH: usize = 64;

    pub fn run(name: &str) -> Result<(), pw::Error> {
        pw::init();
        let main_loop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&main_loop, None)?;
        let core = context.connect_rc(None)?;
        let stream = pw::stream::StreamRc::new(
            core,
            name,
            pw::properties::properties! {
                *pw::keys::NODE_NAME => name,
                *pw::keys::NODE_DESCRIPTION => "StreamForge virtual camera",
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CLASS => "Video/Source",
                *pw::keys::MEDIA_ROLE => "Camera",
            },
        )?;
        let buffers = buffers_pod();
        let frame = Cell::new(0_usize);
        let _listener = stream
            .add_local_listener_with_user_data(())
            .param_changed(move |stream, _, id, param| {
                if id == ParamType::Format.as_raw() && param.is_some() {
                    let pod = Pod::from_bytes(&buffers).expect("buffers pod");
                    if let Err(error) = stream.update_params(&mut [pod]) {
                        eprintln!("buffer params: {error}");
                    }
                }
            })
            .process(move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let bar = (frame.get() * 16) % WIDTH;
                frame.set(frame.get() + 1);
                let Some(memory) = data.data() else {
                    return;
                };
                if memory.len() < WIDTH * HEIGHT * 2 {
                    return;
                }
                draw(&mut memory[..WIDTH * HEIGHT * 2], bar);
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.size_mut() = (WIDTH * HEIGHT * 2) as u32;
                *chunk.stride_mut() = (WIDTH * 2) as i32;
            })
            .register()?;
        let format = format_pod();
        stream.connect(
            pw::spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [Pod::from_bytes(&format).expect("format pod")],
        )?;
        // A driver stream runs when it is triggered: 30 times a second.
        let timer = main_loop.loop_().add_timer({
            let stream = stream.clone();
            move |_| {
                let _ = stream.trigger_process();
            }
        });
        timer
            .update_timer(
                Some(Duration::from_millis(1)),
                Some(Duration::from_nanos(1_000_000_000 / 30)),
            )
            .into_sync_result()?;
        eprintln!("virtual camera `{name}` runs; stop it with Ctrl-C");
        main_loop.run();
        Ok(())
    }

    /// One YUY2 frame: red left, blue right, a white bar from column `bar`.
    fn draw(frame: &mut [u8], bar: usize) {
        let row: Vec<u8> = (0..WIDTH)
            .step_by(2)
            .flat_map(|x| {
                let [y, u, v] = if (bar..bar + BAR_WIDTH).contains(&x) {
                    WHITE
                } else if x < WIDTH / 2 {
                    RED
                } else {
                    BLUE
                };
                [y, u, y, v]
            })
            .collect();
        for line in frame.as_chunks_mut::<{ WIDTH * 2 }>().0 {
            line.copy_from_slice(&row);
        }
    }

    fn format_pod() -> Vec<u8> {
        use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
        use pw::spa::utils::{Fraction, Id, Rectangle, SpaTypes};

        let id =
            |key: FormatProperties, value: u32| Property::new(key.as_raw(), Value::Id(Id(value)));
        serialize(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: vec![
                id(FormatProperties::MediaType, MediaType::Video.as_raw()),
                id(FormatProperties::MediaSubtype, MediaSubtype::Raw.as_raw()),
                id(
                    FormatProperties::VideoFormat,
                    pw::spa::sys::SPA_VIDEO_FORMAT_YUY2,
                ),
                Property::new(
                    FormatProperties::VideoSize.as_raw(),
                    Value::Rectangle(Rectangle {
                        width: WIDTH as u32,
                        height: HEIGHT as u32,
                    }),
                ),
                Property::new(
                    FormatProperties::VideoFramerate.as_raw(),
                    Value::Fraction(Fraction { num: 30, denom: 1 }),
                ),
            ],
        })
    }

    fn buffers_pod() -> Vec<u8> {
        let int = |key: u32, value: i32| Property::new(key, Value::Int(value));
        serialize(Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
            id: ParamType::Buffers.as_raw(),
            properties: vec![
                int(pw::spa::sys::SPA_PARAM_BUFFERS_buffers, 4),
                int(pw::spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
                int(
                    pw::spa::sys::SPA_PARAM_BUFFERS_size,
                    (WIDTH * HEIGHT * 2) as i32,
                ),
                int(pw::spa::sys::SPA_PARAM_BUFFERS_stride, (WIDTH * 2) as i32),
            ],
        })
    }

    fn serialize(object: Object) -> Vec<u8> {
        pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &Value::Object(object),
        )
        .expect("serialize a pod")
        .0
        .into_inner()
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    {
        let name = std::env::args()
            .nth(1)
            .unwrap_or_else(|| "streamforge-virtual-camera".to_owned());
        if let Err(error) = camera::run(&name) {
            eprintln!("virtual camera: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "linux"))]
    eprintln!("the virtual camera needs PipeWire, which is Linux only");
}
