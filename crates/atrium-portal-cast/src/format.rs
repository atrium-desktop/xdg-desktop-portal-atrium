//! Compositor wire-format mapping and the SPA format/buffers parameter pods
//! the stream offers and parses.

use pipewire as pw;
use pw::spa;
use pw::spa::pod::{self, Pod};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};

/// Framerate choice offered to PipeWire consumers. Frames arrive at the
/// compositor's actual cadence; the range lets each consumer pick the rate
/// its own pipeline wants instead of forcing one fixed clock on everyone.
pub(crate) const FRAMERATE_DEFAULT: Fraction = Fraction { num: 60, denom: 1 };
pub(crate) const FRAMERATE_MIN: Fraction = Fraction { num: 1, denom: 1 };
pub(crate) const FRAMERATE_MAX: Fraction = Fraction { num: 360, denom: 1 };
/// `SPA_PARAM_BUFFERS_dataType` is a mask of `1 << SPA_DATA_*`: MemPtr is
/// bit 1, MemFd is bit 2, and DmaBuf is bit 3.
pub(crate) const DMABUF_DATA_TYPE_BIT: u32 = 1 << 3;
pub(crate) const ALL_DATA_TYPES: u32 = (1 << 1) | (1 << 2) | DMABUF_DATA_TYPE_BIT;
/// Pool depth offered to copy-path consumers. Encoder consumers hold
/// buffers while their reorder lookahead runs, so the two-buffer minimum
/// would drop every frame a consumer holds even briefly; four absorbs the
/// hold. Zero-copy slot streams default to the slot count instead — every
/// pool buffer binds a slot, so extras would only serve the copy fallback.
pub(crate) const SHM_POOL_BUFFERS: usize = 4;
/// DRM_FORMAT_MOD_LINEAR: the only dmabuf layout the copy path may
/// memory-map. Tiled layouts read back tile-swizzled on the CPU, so they
/// must come from the compositor's SHM readback instead.
pub(crate) const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// The pixel format the compositor announced at `StreamOutputStart`, with
/// the PipeWire-side mapping resolved once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnouncedFormat {
    /// Sealed-memfd SHM frames; the value is the SPA raw format offered to
    /// PipeWire (`BGRx` for Bgra8, `RGBx` for Rgba8; compositor alpha is
    /// always opaque).
    Shm(spa::param::video::VideoFormat),
    /// Single-plane dmabuf frames with a fixed DRM format/modifier pair;
    /// the value is the equivalent SPA raw format.
    Dmabuf {
        drm_format: u32,
        modifier: u64,
        spa_format: spa::param::video::VideoFormat,
    },
}

impl AnnouncedFormat {
    pub(crate) fn spa_format(&self) -> spa::param::video::VideoFormat {
        match *self {
            AnnouncedFormat::Shm(format)
            | AnnouncedFormat::Dmabuf {
                spa_format: format, ..
            } => format,
        }
    }
}

/// Resolve the compositor's announced wire format into an offerable SPA
/// format. Unknown dmabuf fourccs fail the cast: offering a guessed pixel
/// layout would produce wrong colors in the consumer.
pub(crate) fn announced_format(
    format: atrium_portal_ipc::StreamPixelFormat,
) -> Result<AnnouncedFormat, String> {
    use atrium_portal_ipc::StreamPixelFormat as Wire;
    match format {
        Wire::Bgra8 => Ok(AnnouncedFormat::Shm(spa::param::video::VideoFormat::BGRx)),
        Wire::Rgba8 => Ok(AnnouncedFormat::Shm(spa::param::video::VideoFormat::RGBx)),
        Wire::Dmabuf {
            drm_format,
            modifier,
        } => {
            let spa_format = spa_format_for_drm(drm_format).ok_or_else(|| {
                format!("unsupported compositor dmabuf format {drm_format:#010x}")
            })?;
            Ok(AnnouncedFormat::Dmabuf {
                drm_format,
                modifier,
                spa_format,
            })
        }
    }
}

/// Little-endian DRM fourcc, matching `drm_fourcc.h`'s `fourcc_code`.
pub(crate) const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// Map a single-plane 8-bit DRM fourcc to the SPA raw video format with the
/// same memory order (DRM names read most- to least-significant byte, SPA
/// names read in memory order).
pub(crate) fn spa_format_for_drm(drm_format: u32) -> Option<spa::param::video::VideoFormat> {
    use spa::param::video::VideoFormat;
    Some(match drm_format {
        f if f == fourcc(b'X', b'R', b'2', b'4') => VideoFormat::BGRx,
        f if f == fourcc(b'A', b'R', b'2', b'4') => VideoFormat::BGRA,
        f if f == fourcc(b'X', b'B', b'2', b'4') => VideoFormat::RGBx,
        f if f == fourcc(b'A', b'B', b'2', b'4') => VideoFormat::RGBA,
        f if f == fourcc(b'R', b'X', b'2', b'4') => VideoFormat::xBGR,
        f if f == fourcc(b'R', b'A', b'2', b'4') => VideoFormat::ABGR,
        f if f == fourcc(b'B', b'X', b'2', b'4') => VideoFormat::xRGB,
        f if f == fourcc(b'B', b'A', b'2', b'4') => VideoFormat::ARGB,
        _ => return None,
    })
}

/// The fixated `SPA_PARAM_Format`, reduced to what delivery needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FixatedFormat {
    pub(crate) spa_format: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) modifier: Option<u64>,
}

/// The default of a SPA choice; fixation may leave any choice kind behind.
pub(crate) fn choice_default<T: Copy + pod::CanonicalFixedSizedPod>(choice: &Choice<T>) -> T {
    match &choice.1 {
        ChoiceEnum::None(value) => *value,
        ChoiceEnum::Range { default, .. } => *default,
        ChoiceEnum::Step { default, .. } => *default,
        ChoiceEnum::Enum { default, .. } => *default,
        ChoiceEnum::Flags { default, .. } => *default,
    }
}

fn pod_value_id(value: &pod::Value) -> Option<u32> {
    match value {
        pod::Value::Id(id) => Some(id.0),
        pod::Value::Choice(pod::ChoiceValue::Id(choice)) => Some(choice_default(choice).0),
        _ => None,
    }
}

fn pod_value_rectangle(value: &pod::Value) -> Option<Rectangle> {
    match value {
        pod::Value::Rectangle(rectangle) => Some(*rectangle),
        pod::Value::Choice(pod::ChoiceValue::Rectangle(choice)) => Some(choice_default(choice)),
        _ => None,
    }
}

/// Parse a fixated `SPA_PARAM_Format` raw-video pod, tolerating both plain
/// and choice-wrapped property values.
pub(crate) fn parse_format_param(param: &Pod) -> Option<FixatedFormat> {
    let value = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(param.as_bytes())
        .ok()?
        .1;
    let pod::Value::Object(object) = value else {
        return None;
    };
    let mut format = None;
    let mut size = None;
    let mut modifier = None;
    for property in &object.properties {
        if property.key == spa::param::format::FormatProperties::VideoFormat.as_raw() {
            format = pod_value_id(&property.value);
        } else if property.key == spa::param::format::FormatProperties::VideoSize.as_raw() {
            size = pod_value_rectangle(&property.value);
        } else if property.key == spa::param::format::FormatProperties::VideoModifier.as_raw() {
            modifier = pod_value_long(&property.value).map(|raw| raw as u64);
        }
    }
    Some(FixatedFormat {
        spa_format: format?,
        width: size?.width,
        height: size?.height,
        modifier,
    })
}

fn pod_value_long(value: &pod::Value) -> Option<i64> {
    match value {
        pod::Value::Long(long) => Some(*long),
        pod::Value::Choice(pod::ChoiceValue::Long(choice)) => Some(choice_default(choice)),
        _ => None,
    }
}

/// Extract the accepted `SPA_PARAM_BUFFERS_dataType` mask from a fixated
/// Buffers param.
pub(crate) fn parse_buffers_data_types(param: &Pod) -> Option<u32> {
    let value = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(param.as_bytes())
        .ok()?
        .1;
    let pod::Value::Object(object) = value else {
        return None;
    };
    for property in &object.properties {
        if property.key != 6 {
            // SPA_PARAM_BUFFERS_dataType
            continue;
        }
        return match &property.value {
            pod::Value::Int(mask) => Some(*mask as u32),
            pod::Value::Choice(pod::ChoiceValue::Int(choice)) => {
                Some(choice_default(choice) as u32)
            }
            _ => None,
        };
    }
    None
}

/// The offered video format: raw video in the SPA format matching the
/// compositor's pixel layout at the output's geometry. The framerate is a
/// range: frames arrive at the negotiated cadence, paced by the compositor
/// and bounded by its vertical sync, and each consumer picks the rate its
/// pipeline wants.
pub(crate) fn format_pod(
    width: u32,
    height: u32,
    spa_format: spa::param::video::VideoFormat,
    modifier: Option<u64>,
) -> Vec<u8> {
    let mut properties = vec![
        pod::Property {
            key: spa::param::format::FormatProperties::MediaType.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Id(spa::utils::Id(
                spa::param::format::MediaType::Video.as_raw(),
            )),
        },
        pod::Property {
            key: spa::param::format::FormatProperties::MediaSubtype.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Id(spa::utils::Id(
                spa::param::format::MediaSubtype::Raw.as_raw(),
            )),
        },
        pod::Property {
            key: spa::param::format::FormatProperties::VideoFormat.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Id(spa::utils::Id(spa_format.as_raw())),
        },
    ];
    if let Some(modifier) = modifier {
        // The Long choice Enum carries exactly one modifier: the one the
        // compositor's slots have. `property!`'s Choice arms do not compile
        // for Long, so the property is built by hand. MANDATORY keeps
        // modifier-ignorant consumers off this entry: they fixate the plain
        // entry instead of fixating a dmabuf format they cannot serve.
        properties.push(pod::Property {
            key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags: pod::PropertyFlags::MANDATORY,
            value: pod::Value::Choice(pod::ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: modifier as i64,
                    alternatives: vec![modifier as i64],
                },
            ))),
        });
    }
    properties.extend([
        pod::Property {
            key: spa::param::format::FormatProperties::VideoSize.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Rectangle(Rectangle { width, height }),
        },
        pod::Property {
            key: spa::param::format::FormatProperties::VideoFramerate.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Choice(pod::ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: FRAMERATE_DEFAULT,
                    min: FRAMERATE_MIN,
                    max: FRAMERATE_MAX,
                },
            ))),
        },
    ]);
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    };
    serialize(&pod::Value::Object(object))
}

/// The format set offered at connect time. A slot stream offers its
/// modifier entry first (preferred by GPU consumers) and an equivalent
/// plain entry as the universal fallback; everything else offers only the
/// plain entry, because per-frame descriptors cannot populate DmaBuf pool
/// buffers (see the module docs).
pub(crate) fn format_pods(
    width: u32,
    height: u32,
    announced: AnnouncedFormat,
    has_slots: bool,
) -> Vec<Vec<u8>> {
    let mut pods = Vec::new();
    if let AnnouncedFormat::Dmabuf {
        spa_format,
        modifier,
        ..
    } = announced
        && has_slots
    {
        pods.push(format_pod(width, height, spa_format, Some(modifier)));
        pods.push(format_pod(width, height, spa_format, None));
        return pods;
    }
    pods.push(format_pod(width, height, announced.spa_format(), None));
    pods
}

/// Buffer constraints offered once the format is negotiated: buffers of
/// exactly one frame at the layout delivery actually uses (the slot's
/// stride and size for zero-copy dmabuf, tightly packed for the copy
/// path). `default_buffers` is the offered pool depth — the slot count on
/// zero-copy slot streams, `SHM_POOL_BUFFERS` on the copy path.
pub(crate) fn buffers_pod(default_buffers: usize, stride: i32, size: i32) -> Vec<u8> {
    let default = u32::try_from(default_buffers).unwrap_or(0).clamp(2, 8);
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            pod::Property {
                key: 1, // SPA_PARAM_BUFFERS_buffers
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: default as i32,
                        min: 2,
                        max: 8,
                    },
                ))),
            },
            pod::Property {
                key: 2, // SPA_PARAM_BUFFERS_blocks
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(1),
            },
            pod::Property {
                key: 3, // SPA_PARAM_BUFFERS_size
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(size),
            },
            pod::Property {
                key: 4, // SPA_PARAM_BUFFERS_stride
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(stride),
            },
            pod::Property {
                key: 6, // SPA_PARAM_BUFFERS_dataType
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: ALL_DATA_TYPES as i32,
                        flags: Vec::new(),
                    },
                ))),
            },
        ],
    };
    serialize(&pod::Value::Object(object))
}

fn serialize(value: &pod::Value) -> Vec<u8> {
    pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
        .expect("pod serialization")
        .0
        .into_inner()
}
