use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::HashMap,
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::io::IntoRawFd,
    },
    rc::Rc,
};

use anyhow::Context;
use drm_fourcc::DrmModifier;
use gbm::BufferObjectFlags;
use gl_bindings::gl;
use pipewire::{
    loop_::IoSource,
    properties::properties,
    spa::{
        self,
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            ParamType,
        },
        pod::{
            deserialize::{PodDeserialize, PodDeserializer},
            serialize::{PodSerialize, PodSerializer},
            Pod, PropertyFlags,
        },
        support::system::IoFlags,
        utils::{Direction, SpaTypes},
    },
    stream::StreamFlags,
};
use slotmap::{DefaultKey, SlotMap};
use smallvec::smallvec;

use crate::{DrmRenderNode, MessagesFromPipewire, MessagesToPipewire};

pub(crate) fn fourcc_to_spa_format(
    fourcc: drm_fourcc::DrmFourcc,
) -> Option<spa::param::video::VideoFormat> {
    use drm_fourcc::DrmFourcc::*;
    use spa::param::video::VideoFormat;
    Some(match fourcc {
        Rgb888 => VideoFormat::BGR,
        Bgr888 => VideoFormat::RGB,
        Bgra8888 => VideoFormat::ARGB,
        Abgr8888 => VideoFormat::RGBA,
        Rgba8888 => VideoFormat::ABGR,
        Argb8888 => VideoFormat::BGRA,
        Bgrx8888 => VideoFormat::xRGB,
        Xbgr8888 => VideoFormat::RGBx,
        Xrgb8888 => VideoFormat::BGRx,
        Rgbx8888 => VideoFormat::xBGR,
        Abgr2101010 => VideoFormat::ABGR_210LE,
        Xbgr2101010 => VideoFormat::xBGR_210LE,
        Argb2101010 => VideoFormat::ARGB_210LE,
        Xrgb2101010 => VideoFormat::xRGB_210LE,
        _ => return None,
    })
}

fn spa_format_to_fourcc(format: spa::param::video::VideoFormat) -> drm_fourcc::DrmFourcc {
    use drm_fourcc::DrmFourcc::*;
    use spa::param::video::VideoFormat;
    match format {
        VideoFormat::RGB => Bgr888,
        VideoFormat::BGR => Rgb888,
        VideoFormat::BGRA => Argb8888,
        VideoFormat::ABGR => Rgba8888,
        VideoFormat::RGBA => Abgr8888,
        VideoFormat::ARGB => Bgra8888,
        VideoFormat::BGRx => Xrgb8888,
        VideoFormat::xBGR => Rgbx8888,
        VideoFormat::xRGB => Bgrx8888,
        VideoFormat::RGBx => Xbgr8888,
        VideoFormat::ABGR_210LE => Abgr2101010,
        VideoFormat::xBGR_210LE => Xbgr2101010,
        VideoFormat::ARGB_210LE => Argb2101010,
        VideoFormat::xRGB_210LE => Xrgb2101010,
        _ => unimplemented!(),
    }
}

#[allow(dead_code)]
fn blit(
    gl: &gl::Gl,
    program: gl::types::GLuint,
    source: gl::types::GLuint,
    target: gl::types::GLuint,
) {
    unsafe {
        gl.UseProgram(program);
        gl.ActiveTexture(gl::TEXTURE0);
        gl.BindTexture(gl::TEXTURE_2D, source);
        gl.Uniform1i(gl.GetUniformLocation(program, c"tex".as_ptr()), 0);
        gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, target);

        let mut vao = 0;
        gl.GenVertexArrays(1, &mut vao);
        gl.BindVertexArray(vao);
        let mut vbo = [0; 2];
        gl.GenBuffers(2, vbo.as_mut_ptr());
        gl.BindBuffer(gl::ARRAY_BUFFER, vbo[0]);
        gl.BindBuffer(gl::ELEMENT_ARRAY_BUFFER, vbo[1]);
        let vertex: [gl::types::GLfloat; 8] = [0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0];
        gl.BufferData(
            gl::ARRAY_BUFFER,
            (4 * 2) as isize,
            vertex.as_ptr() as *const std::ffi::c_void,
            gl::STATIC_DRAW,
        );
        gl.BufferData(
            gl::ELEMENT_ARRAY_BUFFER,
            (6 * 2) as isize,
            [0, 1, 2, 0, 2, 3].as_ptr() as *const std::ffi::c_void,
            gl::STATIC_DRAW,
        );
        gl.EnableVertexAttribArray(0);
        gl.VertexAttribPointer(
            0,
            2,
            gl::FLOAT,
            gl::FALSE,
            (std::mem::size_of::<gl::types::GLfloat>() * 2) as _,
            std::ptr::null(),
        );
        gl.DrawElements(gl::TRIANGLES, 6, gl::UNSIGNED_INT, std::ptr::null());

        gl.BindFramebuffer(gl::DRAW_FRAMEBUFFER, 0);
    }
}

#[derive(Debug)]
struct ParamFormat<'a> {
    format:    Option<spa::param::video::VideoFormat>,
    modifiers: Cow<'a, [DrmModifier]>,
    width:     u32,
    height:    u32,
    fixate:    bool,
}

impl<'a> ParamFormat<'a> {
    fn satisfies(&self, other: &ParamFormat<'_>) -> bool {
        self.format == other.format
            && self.width == other.width
            && self.height == other.height
            && self.modifiers.iter().all(|m| other.modifiers.contains(m))
    }
}

impl<'a> PodDeserialize<'a> for ParamFormat<'static> {
    fn deserialize(
        deserializer: spa::pod::deserialize::PodDeserializer<'a>,
    ) -> Result<
        (Self, spa::pod::deserialize::DeserializeSuccess<'a>),
        spa::pod::deserialize::DeserializeError<&'a [u8]>,
    >
    where
        Self: Sized,
    {
        struct Visitor;
        impl<'de> spa::pod::deserialize::Visitor<'de> for Visitor {
            type ArrayElem = std::convert::Infallible;
            type Value = ParamFormat<'static>;

            fn visit_object(
                &self,
                object_deserializer: &mut spa::pod::deserialize::ObjectPodDeserializer<'de>,
            ) -> Result<Self::Value, spa::pod::deserialize::DeserializeError<&'de [u8]>>
            {
                use spa::{
                    pod::{ChoiceValue, Value},
                    utils::{Choice, ChoiceEnum, Id},
                };
                let mut ret = ParamFormat {
                    format:    None,
                    modifiers: Cow::Borrowed(&[]),
                    width:     0,
                    height:    0,
                    fixate:    false,
                };
                let modifiers = ret.modifiers.to_mut();
                while let Some((prop, id, flags)) =
                    object_deserializer.deserialize_property::<Value>()?
                {
                    let id = FormatProperties::from_raw(id);
                    match id {
                        FormatProperties::VideoSize => {
                            match prop {
                                Value::Rectangle(rect) => {
                                    ret.width = rect.width;
                                    ret.height = rect.height;
                                }
                                Value::Choice(ChoiceValue::Rectangle(Choice(
                                    _flags,
                                    ChoiceEnum::None(rect),
                                ))) => {
                                    ret.width = rect.width;
                                    ret.height = rect.height;
                                }
                                _ => {
                                    tracing::debug!("Invalid type for VideoSize: {:?}", prop);
                                    return Err(
                                        spa::pod::deserialize::DeserializeError::InvalidType,
                                    );
                                }
                            }
                        }
                        FormatProperties::VideoModifier => {
                            ret.fixate = !flags.contains(PropertyFlags::DONT_FIXATE);
                            match prop {
                                Value::Long(m) => {
                                    modifiers.push(DrmModifier::from(m as u64));
                                }
                                Value::Choice(ChoiceValue::Long(Choice(_flags, choices))) => {
                                    match choices {
                                        ChoiceEnum::Enum { default, alternatives } => {
                                            modifiers.push(DrmModifier::from(default as u64));
                                            modifiers.extend(
                                                alternatives
                                                    .iter()
                                                    .map(|&m| DrmModifier::from(m as u64)),
                                            );
                                        }
                                        ChoiceEnum::None(m) => {
                                            modifiers.push(DrmModifier::from(m as u64));
                                        }
                                        _ => {
                                            tracing::debug!(
                                                "Invalid choices for VideoModifier: {:?}",
                                                choices
                                            );
                                            return Err(
                                                spa::pod::deserialize::DeserializeError::InvalidChoiceType,
                                            );
                                        }
                                    }
                                }
                                _ => {
                                    tracing::debug!("Invalid type for VideoModifier: {:?}", prop);
                                    return Err(
                                        spa::pod::deserialize::DeserializeError::InvalidChoiceType,
                                    );
                                }
                            }
                        }
                        FormatProperties::VideoFormat => {
                            match prop {
                                Value::Id(id) => {
                                    let format = spa::param::video::VideoFormat::from_raw(id.0);
                                    ret.format = Some(format);
                                }
                                Value::Choice(ChoiceValue::Id(Choice(
                                    _,
                                    ChoiceEnum::None(Id(format)),
                                ))) => {
                                    let format = spa::param::video::VideoFormat::from_raw(format);
                                    ret.format = Some(format);
                                }
                                _ => {
                                    tracing::debug!("Invalid type for VideoFormat: {:?}", prop);
                                    return Err(
                                        spa::pod::deserialize::DeserializeError::InvalidType,
                                    );
                                }
                            }
                        }

                        _ => {
                            tracing::debug!("Unknown property: {:?}", id);
                        }
                    }
                }
                Ok(ret)
            }
        }
        deserializer.deserialize_object(Visitor)
    }
}

impl<'a> PodSerialize for ParamFormat<'a> {
    fn serialize<O: std::io::Write + std::io::Seek>(
        &self,
        serializer: PodSerializer<O>,
    ) -> Result<spa::pod::serialize::SerializeSuccess<O>, spa::pod::serialize::GenError> {
        use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};
        assert!(!self.fixate || self.modifiers.len() == 1);
        let mut serializer =
            serializer.serialize_object(SpaTypes::ObjectParamFormat.0, ParamType::EnumFormat.0)?;
        serializer.serialize_property(
            FormatProperties::MediaType.0,
            &Id(MediaType::Video.0),
            PropertyFlags::empty(),
        )?;
        serializer.serialize_property(
            FormatProperties::MediaSubtype.0,
            &Id(MediaSubtype::Raw.0),
            PropertyFlags::empty(),
        )?;
        if let Some(format) = self.format {
            serializer.serialize_property(
                FormatProperties::VideoFormat.0,
                &Id(format.0),
                PropertyFlags::empty(),
            )?;
        }
        if self.modifiers.len() == 1 && self.modifiers[0] == DrmModifier::Invalid {
            serializer.serialize_property(
                FormatProperties::VideoModifier.0,
                &(u64::from(DrmModifier::Invalid) as i64),
                PropertyFlags::MANDATORY,
            )?;
        } else {
            let mut modifiers: Vec<_> =
                self.modifiers.iter().map(|m| u64::from(*m) as i64).collect();
            if !self.modifiers.contains(&DrmModifier::Invalid) && !self.fixate {
                modifiers.push(u64::from(DrmModifier::Invalid) as _);
            }
            let choice = Choice(ChoiceFlags::empty(), ChoiceEnum::Enum {
                default:      u64::from(self.modifiers[0]) as _,
                alternatives: modifiers,
            });
            let flags = if self.fixate {
                PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE
            } else {
                PropertyFlags::MANDATORY
            };
            serializer.serialize_property(FormatProperties::VideoModifier.0, &choice, flags)?;
        }
        serializer.serialize_property(
            FormatProperties::VideoSize.0,
            &Rectangle { width: self.width, height: self.height },
            PropertyFlags::empty(),
        )?;
        serializer.serialize_property(
            FormatProperties::VideoFramerate.0,
            &Fraction { num: 0, denom: 1 }, // Variable framerate?
            PropertyFlags::empty(),
        )?;
        serializer.end()
    }
}

struct BufferInfo<'a> {
    buffers:   (u32, u32),
    blocks:    u32,
    size:      u32,
    stride:    u32,
    data_type: &'a [spa::buffer::DataType],
}

impl<'a> PodSerialize for BufferInfo<'a> {
    fn serialize<O: std::io::prelude::Write + std::io::prelude::Seek>(
        &self,
        serializer: PodSerializer<O>,
    ) -> Result<spa::pod::serialize::SerializeSuccess<O>, spa::pod::serialize::GenError> {
        use spa::{
            sys,
            utils::{Choice, ChoiceEnum, ChoiceFlags},
        };
        let mut serializer =
            serializer.serialize_object(SpaTypes::ObjectParamBuffers.0, ParamType::Buffers.0)?;
        if self.buffers.0 != self.buffers.1 {
            serializer.serialize_property(
                sys::SPA_PARAM_BUFFERS_buffers,
                &Choice::<i32>(ChoiceFlags::empty(), ChoiceEnum::Range {
                    default: self.buffers.1 as _,
                    min:     self.buffers.0 as _,
                    max:     self.buffers.1 as _,
                }),
                PropertyFlags::empty(),
            )?;
        } else {
            serializer.serialize_property(
                sys::SPA_PARAM_BUFFERS_buffers,
                &(self.buffers.0 as i32),
                PropertyFlags::empty(),
            )?;
        }
        serializer.serialize_property(
            sys::SPA_PARAM_BUFFERS_blocks,
            &(self.blocks as i32),
            PropertyFlags::empty(),
        )?;
        serializer.serialize_property(
            sys::SPA_PARAM_BUFFERS_size,
            &(self.size as i32),
            PropertyFlags::empty(),
        )?;
        serializer.serialize_property(
            sys::SPA_PARAM_BUFFERS_stride,
            &(self.stride as i32),
            PropertyFlags::empty(),
        )?;

        let data_type_choices = Choice::<i32>(ChoiceFlags::empty(), ChoiceEnum::Flags {
            default: (1 << self.data_type[0].as_raw()) as _,
            flags:   self.data_type.iter().map(|t| (1 << t.as_raw()) as _).collect(),
        });
        serializer.serialize_property(
            sys::SPA_PARAM_BUFFERS_dataType,
            &data_type_choices,
            PropertyFlags::empty(),
        )?;
        serializer.end()
    }
}

type Incoming = MessagesToPipewire;
type Outgoing = MessagesFromPipewire;
type Tx = std::sync::mpsc::Sender<Outgoing>;
type Rx = std::sync::mpsc::Receiver<Incoming>;

struct StreamData {
    this: Rc<Pipewire>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    outstanding_buffer: Cell<Option<*mut pipewire::sys::pw_buffer>>,
    fixated_format: RefCell<Option<ParamFormat<'static>>>,
    buffers: RefCell<HashMap<DefaultKey, gbm::BufferObject<()>>>,
    /// Test allocation buffer
    test_buffer: RefCell<Option<gbm::BufferObject<()>>>,

    reply: Option<oneshot::Sender<Result<u32, anyhow::Error>>>,
}

struct StreamHandle {
    stream:   pipewire::stream::Stream,
    listener: Option<pipewire::stream::StreamListener<StreamData>>,
}
struct Pipewire {
    mainloop: pipewire::main_loop::MainLoop,
    formats_modifiers: Vec<(spa::param::video::VideoFormat, Vec<DrmModifier>)>,
    core: pipewire::core::Core,
    gbm: gbm::Device<DrmRenderNode>,

    streams: RefCell<SlotMap<DefaultKey, StreamHandle>>,

    /// SAFETY: This `IoSource` cannot outlive `self.mainloop`
    fence_waits: RefCell<HashMap<DefaultKey, *mut IoSource<'static, OwnedFd>>>,

    tx: Tx,
    rx: Rx,
}
fn stream_set_error(stream: &pipewire::stream::StreamRef, err: impl std::fmt::Debug, tx: &Tx) {
    let err = format!("Error: {:?}", err);
    tracing::debug!("Stream error: {}", err);
    let err = std::ffi::CString::new(err).unwrap();
    unsafe { pipewire::sys::pw_stream_set_error(stream.as_raw_ptr(), -libc::EIO, err.as_ptr()) };
    tx.send(Outgoing::WakeMeUp).unwrap();
}
impl Pipewire {
    fn new_stream(
        self: &Rc<Self>,
        width: u32,
        height: u32,
        x: i32,
        y: i32,
        embed_cursor: bool,
        reply: oneshot::Sender<Result<u32, anyhow::Error>>,
    ) -> anyhow::Result<()> {
        let stream_props = properties! {
            "media.class" => "Video/Source",
            "media.name" => "Screen",
            "node.name" => "picom-egl-screencast",
        };
        let stream =
            pipewire::stream::Stream::new(&self.core, "picom-egl-screencast", stream_props)?;
        let pods: Vec<_> = self
            .formats_modifiers
            .iter()
            .map(|(format, modifiers)| {
                let mut buf = Vec::new();
                PodSerializer::serialize(std::io::Cursor::new(&mut buf), &ParamFormat {
                    format: Some(*format),
                    modifiers: modifiers.into(),
                    width,
                    height,
                    fixate: false,
                })?;
                Ok(buf)
            })
            .collect::<Result<_, anyhow::Error>>()?;
        let mut pods: Vec<_> = pods.iter().map(|p| Pod::from_bytes(p).unwrap()).collect();
        stream.connect(
            Direction::Output,
            None,
            StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
            &mut pods,
        )?;
        let data = StreamData {
            outstanding_buffer: Cell::new(None),
            this: self.clone(),
            x,
            y,
            width,
            height,
            fixated_format: RefCell::new(None),
            buffers: Default::default(),
            test_buffer: RefCell::new(None),
            reply: Some(reply),
        };
        let stream_id = self.streams.borrow_mut().insert(StreamHandle { stream, listener: None });
        let listener = self.streams.borrow()[stream_id]
            .stream
            .add_local_listener_with_user_data(data)
            .add_buffer(move |stream, data, buf| {
                data.this
                    .handle_add_buffer(
                        &data.fixated_format.borrow(),
                        &mut data.test_buffer.borrow_mut(),
                        &mut data.buffers.borrow_mut(),
                        unsafe { &mut *buf },
                        stream_id,
                        x,
                        y,
                        embed_cursor,
                    )
                    .unwrap_or_else(|e| stream_set_error(stream, e, &data.this.tx))
            })
            .process(move |stream, data| {
                let buffer = data.outstanding_buffer.take();
                tracing::trace!("Process {buffer:?}");
                if let Some(buffer) = buffer {
                    unsafe { stream.queue_raw_buffer(buffer) };
                }
                data.this.send_buffer(stream, data)
            })
            .state_changed(move |stream, data, old_state, state| {
                tracing::info!("State changed: {:?} -> {:?}", old_state, state);
                if state == pipewire::stream::StreamState::Paused {
                    if let Some(reply) = data.reply.take() {
                        reply.send(Ok(stream.node_id())).unwrap()
                    }
                } else if state == pipewire::stream::StreamState::Streaming {
                    data.this.send_buffer(stream, data)
                }
            })
            .param_changed(
                |stream, StreamData { this, fixated_format, test_buffer, .. }, id, pod| {
                    this.handle_param_changed(
                        &mut fixated_format.borrow_mut(),
                        &mut test_buffer.borrow_mut(),
                        stream,
                        id,
                        pod,
                    )
                    .unwrap_or_else(|e| stream_set_error(stream, e, &this.tx))
                },
            )
            .remove_buffer(|_, StreamData { this, .. }, buf| {
                let buf = unsafe { &mut *buf };
                let id = *unsafe { Box::from_raw(buf.user_data as *mut DefaultKey) };
                tracing::info!("Remove buffer: {id:?}");
                let inner_buf = unsafe { &mut *buf.buffer };
                let datas = unsafe {
                    std::slice::from_raw_parts_mut(inner_buf.datas, inner_buf.n_datas as usize)
                };
                for data in datas {
                    unsafe {
                        OwnedFd::from_raw_fd(data.fd as _);
                        data.fd = -1;
                    }
                }
                this.tx.send(Outgoing::RemoveBuffers { ids: smallvec![id] }).unwrap();
            })
            .register()?;
        self.streams.borrow_mut()[stream_id].listener = Some(listener);
        Ok(())
    }

    fn handle_param_changed(
        &self,
        out_fixated_format: &mut Option<ParamFormat<'static>>,
        out_test_buffer: &mut Option<gbm::BufferObject<()>>,
        stream: &pipewire::stream::StreamRef,
        prop: u32,
        data: Option<&Pod>,
    ) -> anyhow::Result<()> {
        tracing::debug!("Param changed: {:x} {}", prop, data.is_none());
        if prop != spa::sys::SPA_PARAM_Format {
            return Ok(());
        }
        let Some(data) = data else {
            if out_fixated_format.is_some() {
                tracing::debug!("Stream completed successfully");
                stream_set_error(stream, "Format removed, stopping.", &self.tx);
            }
            return Ok(());
        };
        unsafe { spa::sys::spa_debug_format(1, std::ptr::null(), data.as_raw_ptr()) };
        let (_, params): (_, ParamFormat<'_>) = PodDeserializer::deserialize_from(data.as_bytes())
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        tracing::debug!("Format: {params:?}");
        if params.modifiers.len() == 0 {
            return Err(anyhow::anyhow!("Client doesn't support DMA-Buf"));
        }

        if let Some(old_fixated_format) = out_fixated_format {
            if !old_fixated_format.satisfies(&params) {
                return Err(anyhow::anyhow!("Client requested a different format"));
            }
        } else if !params.fixate {
            // Announcing fixated format
            let fixated_params = ParamFormat {
                format:    params.format,
                modifiers: Cow::Owned(vec![params.modifiers[0]]),
                width:     params.width,
                height:    params.height,
                fixate:    true,
            };
            let mut buf = Vec::new();
            PodSerializer::serialize(std::io::Cursor::new(&mut buf), &fixated_params)?;
            let pod = Pod::from_bytes(&buf).unwrap();
            *out_fixated_format = Some(fixated_params);
            stream.update_params(&mut [pod])?;
        } else {
            // Now we have format, try to test allocate a buffer
            tracing::debug!("Test allocation");
            let test_buffer =
                out_test_buffer.insert(self.gbm.create_buffer_object_with_modifiers2(
                    params.width,
                    params.height,
                    spa_format_to_fourcc(
                        params.format.with_context(|| anyhow::anyhow!("Format missing"))?,
                    ),
                    [params.modifiers[0]].into_iter(),
                    BufferObjectFlags::RENDERING,
                )?);
            let stride = test_buffer.stride()?;
            let size = stride * test_buffer.height()?;
            let buffer_info = BufferInfo {
                buffers: (2, 3),
                blocks: test_buffer.plane_count()?,
                size,
                stride,
                data_type: &[spa::buffer::DataType::DmaBuf],
            };
            let mut buf = Vec::new();
            PodSerializer::serialize(std::io::Cursor::new(&mut buf), &buffer_info)?;
            let pod = Pod::from_bytes(&buf).unwrap();
            *out_fixated_format = Some(params);
            stream.update_params(&mut [pod])?;
        }

        Ok(())
    }

    fn handle_add_buffer(
        &self,
        format: &Option<ParamFormat<'_>>,
        test_buffer: &mut Option<gbm::BufferObject<()>>,
        buffers: &mut HashMap<DefaultKey, gbm::BufferObject<()>>,
        buf: &mut pipewire::sys::pw_buffer,
        stream_id: DefaultKey,
        x: i32,
        y: i32,
        embed_cursor: bool,
    ) -> anyhow::Result<()> {
        let Some(format) = format else {
            return Err(anyhow::anyhow!("add_buffer called without a negotiated format"));
        };
        tracing::debug!("Add buffer: {format:?}");
        let inner_buf = unsafe { &mut *buf.buffer };
        let dma_buf = if let Some(test_buffer) = test_buffer.take() {
            test_buffer
        } else {
            self.gbm.create_buffer_object_with_modifiers2(
                format.width,
                format.height,
                spa_format_to_fourcc(format.format.unwrap()),
                format.modifiers.iter().copied(),
                BufferObjectFlags::RENDERING,
            )?
        };
        assert!(inner_buf.n_datas == dma_buf.plane_count()?);
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Outgoing::AddBuffer { dma_buf, stream_id, x, y, embed_cursor, reply: tx })
            .unwrap();
        let Ok((id, dma_buf)) = rx.recv() else {
            panic!("picom failed to add buffer");
        };
        buf.user_data = Box::leak(Box::new(id)) as *mut _ as *mut _;
        let datas =
            unsafe { std::slice::from_raw_parts_mut(inner_buf.datas, inner_buf.n_datas as usize) };
        let height = dma_buf.height()?;
        for (i, data) in datas.iter_mut().enumerate() {
            let stride = dma_buf.stride_for_plane(i as _)?;
            data.fd = dma_buf.fd_for_plane(i as _)?.into_raw_fd() as _;
            data.type_ = spa::sys::SPA_DATA_DmaBuf;
            data.data = std::ptr::null_mut();
            data.maxsize = stride * height;
            data.flags = spa::sys::SPA_DATA_FLAG_READWRITE;
            let chunk = unsafe { &mut *data.chunk };
            chunk.offset = dma_buf.offset(i as _)?;
            chunk.size = stride * height;
            chunk.stride = stride as _;
            chunk.flags = spa::sys::SPA_CHUNK_FLAG_NONE as _;
        }
        buffers.insert(id, dma_buf);
        Ok(())
    }

    fn send_buffer(&self, stream: &pipewire::stream::StreamRef, data: &StreamData) {
        if !matches!(stream.state(), pipewire::stream::StreamState::Streaming)
            || data.outstanding_buffer.get().is_some()
        {
            return;
        }
        let Some(buffer) = (unsafe { stream.dequeue_raw_buffer().as_mut() }) else {
            return;
        };
        let id = unsafe { *(buffer.user_data as *const DefaultKey) };
        data.outstanding_buffer.set(Some(buffer));
        self.tx.send(Outgoing::ActivateBuffer { id }).unwrap();
    }

    fn handle_message(self: &Rc<Self>, msg: Incoming) -> anyhow::Result<()> {
        match msg {
            Incoming::CreateStream { x, y, width, height, reply, embed_cursor } => {
                tracing::debug!("Creating stream with size {}x{}", width, height);
                self.new_stream(width, height, x, y, embed_cursor, reply)?;
            }
            Incoming::NewFrame { id, fence, stream_id } => {
                tracing::trace!("New frame: {id:?} {fence:?} {stream_id:?}");
                let fd = unsafe { OwnedFd::from_raw_fd(u32::from(fence) as _) };
                if self.streams.borrow().get(stream_id).is_some() {
                    let this = self.clone();
                    let io_source = self.mainloop.loop_().add_io(fd, IoFlags::IN, move |_| {
                        tracing::trace!("Fence triggered: {id:?} {stream_id:?}");
                        let streams = this.streams.borrow();
                        if let Some(stream) = streams.get(stream_id) {
                            stream.stream.trigger_process().unwrap_or_else(|e| {
                                stream_set_error(&streams[stream_id].stream, e, &this.tx)
                            });
                        }
                        let _ = unsafe {
                            Box::from_raw(this.fence_waits.borrow_mut().remove(&id).unwrap())
                        };
                    });
                    let io_source = Box::leak(Box::new(io_source)) as *mut IoSource<'_, OwnedFd>;
                    self.fence_waits.borrow_mut().insert(id, io_source.cast());
                }
            }
            Incoming::BufferError { id, stream_id } => {
                tracing::debug!("Buffer error: {id:?} {stream_id:?}");
                let streams = self.streams.borrow();
                stream_set_error(&streams[stream_id].stream, "Buffer error", &self.tx);
            }
            msg => tracing::warn!("Unhandled {msg:?}"),
        }
        Ok(())
    }
}

pub unsafe fn pipewire_main(
    gbm: gbm::Device<DrmRenderNode>,
    waker: pipewire::channel::Receiver<()>,
    rx: std::sync::mpsc::Receiver<crate::MessagesToPipewire>,
    tx: std::sync::mpsc::Sender<crate::MessagesFromPipewire>,

    formats_modifiers: Vec<(spa::param::video::VideoFormat, Vec<DrmModifier>)>,
) -> anyhow::Result<()> {
    pipewire::init();

    tracing::debug!("Starting pipewire thread, #formats {}", formats_modifiers.len());
    for (format, modifiers) in &formats_modifiers {
        tracing::debug!("  format: {format:?}");
        for modifier in modifiers {
            let raw_modifier = u64::from(*modifier);
            let amd_modifier = crate::AmdModifier(raw_modifier);
            tracing::debug!(
                "    modifier: {amd_modifier:x?}, dcc: {}, dcc 64b: {}, dcc 128b {}",
                (raw_modifier >> 13) & 1,
                (raw_modifier >> 16) & 1,
                (raw_modifier >> 17) & 1
            );
        }
    }

    let mainloop = ::pipewire::main_loop::MainLoop::new(None)?;
    let context = pipewire::context::Context::new(&mainloop)?;
    let core = context.connect(None)?;
    let _attached = waker.attach(mainloop.as_ref(), {
        let mainloop = mainloop.clone();
        let pipewire = Rc::new(Pipewire {
            mainloop,
            gbm,
            formats_modifiers,
            fence_waits: Default::default(),
            core,
            streams: Default::default(),
            tx,
            rx,
        });
        move |()| {
            tracing::trace!("Woken");
            loop {
                match pipewire.rx.try_recv() {
                    Ok(msg) => {
                        match pipewire.handle_message(msg) {
                            Ok(()) => {}
                            Err(e) => {
                                tracing::error!("Error handling message: {:?}", e);
                                pipewire.mainloop.quit();
                                return;
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        pipewire.mainloop.quit();
                        return;
                    }
                }
            }
            pipewire.streams.borrow_mut().retain(|_, StreamHandle { stream, .. }| {
                if matches!(stream.state(), pipewire::stream::StreamState::Error(_)) {
                    tracing::info!("Removing errored stream");
                    false
                } else {
                    true
                }
            });
        }
    });
    mainloop.run();
    Ok(())
}
