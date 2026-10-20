//! Reading a built-in class by its Unity type tree (depth, type, name, size, align rows), for
//! the classes whose layouts are too long to write out by hand: PlayerSettings (the game
//! version) and Mesh (its bounds). Layouts change with Unity versions, so each is tied to one.

use std::ops::Range;

use super::unity::Reader;

type Rows<'a> = &'a [(u8, &'a str, &'a str, i32, bool)];

struct Node<'a> {
    ty: &'a str,
    name: &'a str,
    size: i32,
    align: bool,
    children: Vec<Node<'a>>,
}

fn build<'a>(rows: Rows<'a>, at: &mut usize, depth: u8) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    while *at < rows.len() && rows[*at].0 == depth {
        let (_, ty, name, size, align) = rows[*at];
        *at += 1;
        let children = build(rows, at, depth + 1);
        out.push(Node { ty, name, size, align, children });
    }
    out
}

/// Skips one node's data: arrays (an int count, then the items), typeless data (an int count of
/// bytes), structs (their children), fixed-size leaves; then the padding the node asks for.
fn skip(n: &Node, r: &mut Reader) -> Option<()> {
    if n.ty == "TypelessData" {
        let count = r.count()?;
        r.bytes(count)?;
    } else if let Some(array) = n.children.iter().find(|c| c.ty == "Array") {
        let count = r.count()?;
        let item = array.children.get(1)?;
        for _ in 0..count {
            skip(item, r)?;
        }
        if array.align {
            r.align();
        }
    } else if n.children.is_empty() {
        r.bytes(usize::try_from(n.size).ok()?)?;
    } else {
        for c in &n.children {
            skip(c, r)?;
        }
    }
    if n.align {
        r.align();
    }
    Some(())
}

/// The byte range of each top-level field, reading the object by its rows; with `whole`, the
/// rows must account for every byte of it.
fn fields<'a>(rows: Rows<'a>, data: &[u8], whole: bool) -> Result<Vec<(&'a str, Range<usize>)>, String> {
    let nodes = build(rows, &mut 0, 1);
    let mut r = Reader::new(data);
    let mut out = Vec::new();
    for n in &nodes {
        let start = r.pos;
        skip(n, &mut r).ok_or_else(|| format!("the data ends in {}", n.name))?;
        out.push((n.name, start..r.pos));
    }
    if whole && r.pos != data.len() {
        return Err(format!("{} of {} bytes read", r.pos, data.len()));
    }
    Ok(out)
}

fn layout_for<'a>(unity_version: &str, layouts: &[(&str, Rows<'a>)], class: &str) -> Result<Rows<'a>, String> {
    let minor = unity_version.split('.').take(2).collect::<Vec<_>>().join(".");
    layouts.iter().find(|(v, _)| *v == minor).map(|(_, rows)| *rows).ok_or_else(|| format!("no {class} layout for Unity {unity_version} (add one to native/typetree.rs)"))
}

/// PlayerSettings.bundleVersion (Application.version).
pub fn bundle_version(unity_version: &str, data: &[u8]) -> Result<String, String> {
    let rows = layout_for(unity_version, &[("6000.3", PLAYER_SETTINGS_6000_3)], "PlayerSettings")?;
    let fields = fields(rows, data, false).map_err(|e| format!("PlayerSettings is not Unity {unity_version}'s: {e}"))?;
    let range = fields.iter().find(|(n, _)| *n == "bundleVersion").map(|(_, r)| r.clone()).ok_or("PlayerSettings has no bundleVersion")?;
    Reader::new(&data[range]).string().ok_or_else(|| "unreadable bundleVersion".into())
}

/// The byte range of the field at `path` (names from the top level down through structs).
fn locate(nodes: &[Node], r: &mut Reader, path: &[&str]) -> Option<Range<usize>> {
    for n in nodes {
        if n.name == path[0] {
            if path.len() == 1 {
                let start = r.pos;
                skip(n, r)?;
                return Some(start..r.pos);
            }
            return locate(&n.children, r, &path[1..]);
        }
        skip(n, r)?;
    }
    None
}

/// PackedBitVector.UnpackFloats: each item `bits` wide, start + range * q / (2^bits - 1).
/// The item count must fit the data: an error for a bit size over 32, for items without a bit
/// size, or for more items than the data holds.
fn unpack_floats(r: &mut Reader) -> Result<Vec<f32>, String> {
    let unreadable = || "an unreadable PackedBitVector".to_string();
    let (count, range, start) = (r.u32().ok_or_else(unreadable)? as usize, r.f32().ok_or_else(unreadable)?, r.f32().ok_or_else(unreadable)?);
    let n = r.count().ok_or_else(unreadable)?;
    let data = r.bytes(n).ok_or_else(unreadable)?;
    r.align();
    let bits = r.u8().ok_or_else(unreadable)? as u32;
    if bits > 32 {
        return Err(format!("a PackedBitVector of {bits}-bit items"));
    }
    if (count > 0 && bits == 0) || (count as u64) * bits as u64 > n as u64 * 8 {
        return Err(format!("a PackedBitVector of {count} {bits}-bit items in {n} bytes"));
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let max = ((1u64 << bits) - 1) as f32;
    let scale = 1.0 / range;
    let (mut at, mut bit) = (0usize, 0u32);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let (mut x, mut have) = (0u64, 0u32);
        while have < bits {
            x |= ((data[at] as u64) >> bit) << have;
            let take = (bits - have).min(8 - bit);
            bit += take;
            have += take;
            if bit == 8 {
                at += 1;
                bit = 0;
            }
        }
        x &= (1u64 << bits) - 1;
        out.push(x as f32 / (scale * max) + start);
    }
    Ok(out)
}

/// Bytes per component of a VertexFormat (Float32, Float16, UNorm8, SNorm8, UNorm16, SNorm16,
/// UInt8, SInt8, UInt16, SInt16, UInt32, SInt32).
const FORMAT_SIZE: [usize; 12] = [4, 2, 1, 1, 2, 2, 1, 1, 2, 2, 4, 4];

fn half(bits: u16) -> f32 {
    let (sign, exp, man) = ((bits >> 15) as u32, ((bits >> 10) & 0x1f) as i32, (bits & 0x3ff) as f32);
    let v = match exp {
        0 => man * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + man / 1024.0) * 2f32.powi(exp - 15),
    };
    if sign == 1 { -v } else { v }
}

/// Mesh.bounds: center and extent, the box around its vertex positions (float or half-float
/// channels, or a compressed mesh's), else the stored m_LocalAABB.
/// `stream`: the bytes of an external vertex stream (path, offset, size).
pub fn mesh_bounds(unity_version: &str, data: &[u8], stream: &dyn Fn(&str, u64, usize) -> Result<Vec<u8>, String>) -> Result<([f32; 3], [f32; 3]), String> {
    let rows = layout_for(unity_version, &[("6000.3", MESH_6000_3)], "Mesh")?;
    fields(rows, data, true).map_err(|e| format!("a Mesh is not Unity {unity_version}'s: {e}"))?;
    let nodes = build(rows, &mut 0, 1);
    let at = |path: &[&str]| locate(&nodes, &mut Reader::new(data), path).ok_or_else(|| format!("Mesh has no {}", path.join(".")));
    let compressed = unpack_floats(&mut Reader::new(&data[at(&["m_CompressedMesh", "m_Vertices"])?])).map_err(|e| format!("compressed vertices: {e}"))?;
    if compressed.len() % 3 != 0 {
        return Err(format!("{} compressed vertex coordinates", compressed.len()));
    }
    let positions: Vec<[f32; 3]> = if !compressed.is_empty() {
        compressed.chunks_exact(3).map(|v| [v[0], v[1], v[2]]).collect()
    } else {
        let count = Reader::new(&data[at(&["m_VertexData", "m_VertexCount"])?]).u32().ok_or("m_VertexCount")? as usize;
        let mut r = Reader::new(&data[at(&["m_VertexData", "m_Channels"])?]);
        let channels: Vec<[u8; 4]> = (0..r.count().ok_or("m_Channels")?).map(|_| Some([r.u8()?, r.u8()?, r.u8()?, r.u8()? & 0x0f])).collect::<Option<_>>().ok_or("m_Channels")?;
        let mut r = Reader::new(&data[at(&["m_VertexData", "m_DataSize"])?]);
        let n = r.count().ok_or("m_DataSize")?;
        let mut bytes = r.bytes(n).ok_or("m_DataSize")?.to_vec();
        if bytes.is_empty() && count > 0 {
            let mut r = Reader::new(&data[at(&["m_StreamData"])?]);
            let (offset, size) = (r.u64().ok_or("m_StreamData")?, r.u32().ok_or("m_StreamData")? as usize);
            let path = r.string().ok_or("m_StreamData")?;
            bytes = stream(&path, offset, size)?;
        }
        // Streams follow each other, each padded to 16 bytes; a vertex's channels in a stream
        // follow each other.
        let size = |c: &[u8; 4]| FORMAT_SIZE.get(c[2] as usize).map(|s| s * c[3] as usize).ok_or("an unknown vertex format");
        let [stream_of, offset, format, dimension] = *channels.first().ok_or("a mesh without a position channel")?;
        if dimension < 3 || format > 1 {
            return Err(format!("vertex positions as format {format} x{dimension}: not read"));
        }
        let mut start = 0;
        for s in 0..stream_of {
            let stride: usize = channels.iter().filter(|c| c[0] == s && c[3] > 0).map(size).sum::<Result<usize, _>>()?;
            start = (start + stride * count).next_multiple_of(16);
        }
        let stride: usize = channels.iter().filter(|c| c[0] == stream_of && c[3] > 0).map(size).sum::<Result<usize, _>>()?;
        (0..count)
            .map(|i| {
                let at = start + i * stride + offset as usize;
                let component = |k: usize| match format {
                    0 => bytes.get(at + 4 * k..at + 4 * k + 4).map(|b| f32::from_le_bytes(b.try_into().unwrap())),
                    _ => bytes.get(at + 2 * k..at + 2 * k + 2).map(|b| half(u16::from_le_bytes(b.try_into().unwrap()))),
                };
                Some([component(0)?, component(1)?, component(2)?])
            })
            .collect::<Option<_>>()
            .ok_or("vertex data shorter than its vertices")?
    };
    if positions.is_empty() {
        let mut r = Reader::new(&data[at(&["m_LocalAABB"])?]);
        return Ok((r.vec3().ok_or("m_LocalAABB")?, r.vec3().ok_or("m_LocalAABB")?));
    }
    let (mut min, mut max) = ([f32::MAX; 3], [f32::MIN; 3]);
    for v in &positions {
        for i in 0..3 {
            min[i] = min[i].min(v[i]);
            max[i] = max[i].max(v[i]);
        }
    }
    Ok(([0, 1, 2].map(|i| (min[i] + max[i]) * 0.5), [0, 1, 2].map(|i| (max[i] - min[i]) * 0.5)))
}

pub const PLAYER_SETTINGS_6000_3: &[(u8, &str, &str, i32, bool)] = &[
    (1, "GUID", "productGUID", 16, false),
    (2, "unsigned int", "data[0]", 4, false),
    (2, "unsigned int", "data[1]", 4, false),
    (2, "unsigned int", "data[2]", 4, false),
    (2, "unsigned int", "data[3]", 4, false),
    (1, "bool", "AndroidProfiler", 1, false),
    (1, "bool", "AndroidFilterTouchesWhenObscured", 1, false),
    (1, "bool", "AndroidEnableSustainedPerformanceMode", 1, true),
    (1, "int", "defaultScreenOrientation", 4, false),
    (1, "int", "targetDevice", 4, false),
    (1, "bool", "useOnDemandResources", 1, true),
    (1, "int", "accelerometerFrequency", 4, true),
    (1, "string", "companyName", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "char", "data", 1, false),
    (1, "string", "productName", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "char", "data", 1, false),
    (1, "PPtr<Texture2D>", "defaultCursor", 12, false),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "Vector2f", "cursorHotspot", 8, false),
    (2, "float", "x", 4, false),
    (2, "float", "y", 4, false),
    (1, "ColorRGBA", "m_SplashScreenBackgroundColor", 16, false),
    (2, "float", "r", 4, false),
    (2, "float", "g", 4, false),
    (2, "float", "b", 4, false),
    (2, "float", "a", 4, false),
    (1, "bool", "m_ShowUnitySplashScreen", 1, false),
    (1, "bool", "m_ShowUnitySplashLogo", 1, true),
    (1, "float", "m_SplashScreenOverlayOpacity", 4, false),
    (1, "int", "m_SplashScreenAnimation", 4, false),
    (1, "int", "m_SplashScreenLogoStyle", 4, false),
    (1, "int", "m_SplashScreenDrawMode", 4, true),
    (1, "float", "m_SplashScreenBackgroundAnimationZoom", 4, false),
    (1, "float", "m_SplashScreenLogoAnimationZoom", 4, false),
    (1, "float", "m_SplashScreenBackgroundLandscapeAspect", 4, false),
    (1, "float", "m_SplashScreenBackgroundPortraitAspect", 4, false),
    (1, "Rectf", "m_SplashScreenBackgroundLandscapeUvs", 16, false),
    (2, "float", "x", 4, false),
    (2, "float", "y", 4, false),
    (2, "float", "width", 4, false),
    (2, "float", "height", 4, false),
    (1, "Rectf", "m_SplashScreenBackgroundPortraitUvs", 16, false),
    (2, "float", "x", 4, false),
    (2, "float", "y", 4, false),
    (2, "float", "width", 4, false),
    (2, "float", "height", 4, false),
    (1, "vector", "m_SplashScreenLogos", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "SplashScreenLogo", "data", 16, false),
    (4, "PPtr<Sprite>", "logo", 12, false),
    (5, "int", "m_FileID", 4, false),
    (5, "SInt64", "m_PathID", 8, false),
    (4, "float", "duration", 4, true),
    (1, "PPtr<Texture2D>", "m_SplashScreenBackgroundLandscape", 12, false),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "PPtr<Texture2D>", "m_SplashScreenBackgroundPortrait", 12, false),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "PPtr<Sprite>", "m_UnitySplashLogo", 12, false),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "PPtr<Texture2D>", "m_VirtualRealitySplashScreen", 12, true),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "PPtr<Texture2D>", "m_HolographicTrackingLossScreen", 12, false),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "int", "defaultScreenWidth", 4, false),
    (1, "int", "defaultScreenHeight", 4, false),
    (1, "int", "defaultScreenWidthWeb", 4, false),
    (1, "int", "defaultScreenHeightWeb", 4, false),
    (1, "int", "m_StereoRenderingPath", 4, false),
    (1, "int", "m_ActiveColorSpace", 4, false),
    (1, "int", "unsupportedMSAAFallback", 4, false),
    (1, "int", "m_SpriteBatchMaxVertexCount", 4, false),
    (1, "int", "m_SpriteBatchVertexThreshold", 4, false),
    (1, "bool", "m_MTRendering", 1, false),
    (1, "bool", "mobileMTRenderingBaked", 1, false),
    (1, "Hash128", "AID", 16, true),
    (2, "UInt8", "bytes[0]", 1, false),
    (2, "UInt8", "bytes[1]", 1, false),
    (2, "UInt8", "bytes[2]", 1, false),
    (2, "UInt8", "bytes[3]", 1, false),
    (2, "UInt8", "bytes[4]", 1, false),
    (2, "UInt8", "bytes[5]", 1, false),
    (2, "UInt8", "bytes[6]", 1, false),
    (2, "UInt8", "bytes[7]", 1, false),
    (2, "UInt8", "bytes[8]", 1, false),
    (2, "UInt8", "bytes[9]", 1, false),
    (2, "UInt8", "bytes[10]", 1, false),
    (2, "UInt8", "bytes[11]", 1, false),
    (2, "UInt8", "bytes[12]", 1, false),
    (2, "UInt8", "bytes[13]", 1, false),
    (2, "UInt8", "bytes[14]", 1, false),
    (2, "UInt8", "bytes[15]", 1, false),
    (1, "int", "playerMinOpenGLESVersion", 4, false),
    (1, "bool", "mipStripping", 1, true),
    (1, "int", "numberOfMipsStripped", 4, false),
    (1, "map", "numberOfMipsStrippedPerMipmapLimitGroup", -1, false),
    (2, "Array", "Array", -1, false),
    (3, "int", "size", 4, false),
    (3, "pair", "data", -1, false),
    (4, "string", "first", -1, false),
    (5, "Array", "Array", -1, true),
    (6, "int", "size", 4, false),
    (6, "char", "data", 1, false),
    (4, "int", "second", 4, false),
    (1, "vector", "m_StackTraceTypes", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "int", "data", 4, false),
    (1, "int", "iosShowActivityIndicatorOnLoading", 4, false),
    (1, "int", "androidShowActivityIndicatorOnLoading", 4, false),
    (1, "bool", "iosUseCustomAppBackgroundBehavior", 1, false),
    (1, "bool", "allowedAutorotateToPortrait", 1, false),
    (1, "bool", "allowedAutorotateToPortraitUpsideDown", 1, false),
    (1, "bool", "allowedAutorotateToLandscapeRight", 1, false),
    (1, "bool", "allowedAutorotateToLandscapeLeft", 1, false),
    (1, "bool", "useOSAutorotation", 1, false),
    (1, "bool", "use32BitDisplayBuffer", 1, false),
    (1, "bool", "preserveFramebufferAlpha", 1, false),
    (1, "bool", "disableDepthAndStencilBuffers", 1, false),
    (1, "bool", "androidStartInFullscreen", 1, false),
    (1, "bool", "androidRenderOutsideSafeArea", 1, false),
    (1, "bool", "androidUseSwappy", 1, true),
    (1, "int", "androidDisplayOptions", 4, false),
    (1, "int", "androidBlitType", 4, true),
    (1, "bool", "androidResizeableActivity", 1, true),
    (1, "int", "androidDefaultWindowWidth", 4, false),
    (1, "int", "androidDefaultWindowHeight", 4, false),
    (1, "int", "androidMinimumWindowWidth", 4, false),
    (1, "int", "androidMinimumWindowHeight", 4, false),
    (1, "int", "androidFullscreenMode", 4, false),
    (1, "int", "androidAutoRotationBehavior", 4, false),
    (1, "bool", "androidPredictiveBackSupport", 1, true),
    (1, "int", "androidApplicationEntry", 4, false),
    (1, "bool", "defaultIsNativeResolution", 1, false),
    (1, "bool", "macRetinaSupport", 1, false),
    (1, "bool", "runInBackground", 1, false),
    (1, "bool", "muteOtherAudioSources", 1, false),
    (1, "bool", "Prepare IOS For Recording", 1, false),
    (1, "bool", "Force IOS Speakers When Recording", 1, true),
    (1, "int", "audioSpatialExperience", 4, false),
    (1, "int", "deferSystemGesturesMode", 4, false),
    (1, "bool", "hideHomeButton", 1, false),
    (1, "bool", "submitAnalytics", 1, false),
    (1, "bool", "usePlayerLog", 1, false),
    (1, "bool", "dedicatedServerOptimizations", 1, false),
    (1, "bool", "bakeCollisionMeshes", 1, false),
    (1, "bool", "forceSingleInstance", 1, false),
    (1, "bool", "useFlipModelSwapchain", 1, false),
    (1, "bool", "resizableWindow", 1, false),
    (1, "bool", "useMacAppStoreValidation", 1, true),
    (1, "string", "macAppStoreCategory", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "char", "data", 1, false),
    (1, "bool", "gpuSkinning", 1, true),
    (1, "int", "meshDeformation", 4, false),
    (1, "bool", "xboxPIXTextureCapture", 1, false),
    (1, "bool", "xboxEnableAvatar", 1, false),
    (1, "bool", "xboxEnableKinect", 1, false),
    (1, "bool", "xboxEnableKinectAutoTracking", 1, false),
    (1, "bool", "xboxEnableFitness", 1, false),
    (1, "bool", "visibleInBackground", 1, false),
    (1, "bool", "allowFullscreenSwitch", 1, true),
    (1, "int", "fullscreenMode", 4, true),
    (1, "unsigned int", "xboxSpeechDB", 4, false),
    (1, "bool", "xboxEnableHeadOrientation", 1, true),
    (1, "bool", "xboxEnableGuest", 1, true),
    (1, "bool", "xboxEnablePIXSampling", 1, true),
    (1, "bool", "metalFramebufferOnly", 1, true),
    (1, "int", "xboxOneResolution", 4, false),
    (1, "int", "xboxOneSResolution", 4, false),
    (1, "int", "xboxOneXResolution", 4, false),
    (1, "int", "xboxOneMonoLoggingLevel", 4, false),
    (1, "int", "xboxOneLoggingLevel", 4, false),
    (1, "bool", "xboxOneDisableEsram", 1, true),
    (1, "bool", "xboxOneEnableTypeOptimization", 1, true),
    (1, "unsigned int", "xboxOnePresentImmediateThreshold", 4, false),
    (1, "int", "switchQueueCommandMemory", 4, false),
    (1, "int", "switchQueueControlMemory", 4, false),
    (1, "int", "switchQueueComputeMemory", 4, false),
    (1, "int", "switchNVNShaderPoolsGranularity", 4, false),
    (1, "int", "switchNVNDefaultPoolsGranularity", 4, false),
    (1, "int", "switchNVNOtherPoolsGranularity", 4, false),
    (1, "int", "switchGpuScratchPoolGranularity", 4, false),
    (1, "bool", "switchAllowGpuScratchShrinking", 1, true),
    (1, "int", "switchNVNMaxPublicTextureIDCount", 4, false),
    (1, "int", "switchNVNMaxPublicSamplerIDCount", 4, false),
    (1, "int", "switchMaxWorkerMultiple", 4, false),
    (1, "int", "switchNVNGraphicsFirmwareMemory", 4, false),
    (1, "bool", "switchGraphicsJobsSyncAfterKick", 1, true),
    (1, "unsigned int", "vulkanNumSwapchainBuffers", 4, false),
    (1, "bool", "vulkanEnableSetSRGBWrite", 1, false),
    (1, "bool", "vulkanEnablePreTransform", 1, false),
    (1, "bool", "vulkanEnableLateAcquireNextImage", 1, false),
    (1, "bool", "vulkanEnableCommandBufferRecycling", 1, true),
    (1, "bool", "loadStoreDebugModeEnabled", 1, true),
    (1, "PPtr<Texture2D>", "invalidatedPatternTexture", 12, true),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "string", "visionOSBundleVersion", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "char", "data", 1, false),
    (1, "string", "tvOSBundleVersion", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "char", "data", 1, false),
    (1, "string", "bundleVersion", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "char", "data", 1, false),
];

pub const MESH_6000_3: &[(u8, &str, &str, i32, bool)] = &[
    (1, "string", "m_Name", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "char", "data", 1, false),
    (1, "vector", "m_SubMeshes", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "SubMesh", "data", 48, false),
    (4, "unsigned int", "firstByte", 4, false),
    (4, "unsigned int", "indexCount", 4, false),
    (4, "int", "topology", 4, false),
    (4, "unsigned int", "baseVertex", 4, false),
    (4, "unsigned int", "firstVertex", 4, false),
    (4, "unsigned int", "vertexCount", 4, false),
    (4, "AABB", "localAABB", 24, false),
    (5, "Vector3f", "m_Center", 12, false),
    (6, "float", "x", 4, false),
    (6, "float", "y", 4, false),
    (6, "float", "z", 4, false),
    (5, "Vector3f", "m_Extent", 12, false),
    (6, "float", "x", 4, false),
    (6, "float", "y", 4, false),
    (6, "float", "z", 4, false),
    (1, "BlendShapeData", "m_Shapes", -1, false),
    (2, "vector", "vertices", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "BlendShapeVertex", "data", 40, false),
    (5, "Vector3f", "vertex", 12, false),
    (6, "float", "x", 4, false),
    (6, "float", "y", 4, false),
    (6, "float", "z", 4, false),
    (5, "Vector3f", "normal", 12, false),
    (6, "float", "x", 4, false),
    (6, "float", "y", 4, false),
    (6, "float", "z", 4, false),
    (5, "Vector3f", "tangent", 12, false),
    (6, "float", "x", 4, false),
    (6, "float", "y", 4, false),
    (6, "float", "z", 4, false),
    (5, "unsigned int", "index", 4, false),
    (2, "vector", "shapes", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "MeshBlendShape", "data", 10, false),
    (5, "unsigned int", "firstVertex", 4, false),
    (5, "unsigned int", "vertexCount", 4, false),
    (5, "bool", "hasNormals", 1, false),
    (5, "bool", "hasTangents", 1, true),
    (2, "vector", "channels", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "MeshBlendShapeChannel", "data", -1, false),
    (5, "string", "name", -1, false),
    (6, "Array", "Array", -1, true),
    (7, "int", "size", 4, false),
    (7, "char", "data", 1, false),
    (5, "unsigned int", "nameHash", 4, false),
    (5, "int", "frameIndex", 4, false),
    (5, "int", "frameCount", 4, false),
    (2, "vector", "fullWeights", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "float", "data", 4, false),
    (1, "vector", "m_BindPose", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "Matrix4x4f", "data", 64, false),
    (4, "float", "e00", 4, false),
    (4, "float", "e01", 4, false),
    (4, "float", "e02", 4, false),
    (4, "float", "e03", 4, false),
    (4, "float", "e10", 4, false),
    (4, "float", "e11", 4, false),
    (4, "float", "e12", 4, false),
    (4, "float", "e13", 4, false),
    (4, "float", "e20", 4, false),
    (4, "float", "e21", 4, false),
    (4, "float", "e22", 4, false),
    (4, "float", "e23", 4, false),
    (4, "float", "e30", 4, false),
    (4, "float", "e31", 4, false),
    (4, "float", "e32", 4, false),
    (4, "float", "e33", 4, false),
    (1, "vector", "m_BoneNameHashes", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "unsigned int", "data", 4, false),
    (1, "unsigned int", "m_RootBoneNameHash", 4, false),
    (1, "vector", "m_BonesAABB", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "MinMaxAABB", "data", 24, false),
    (4, "Vector3f", "m_Min", 12, false),
    (5, "float", "x", 4, false),
    (5, "float", "y", 4, false),
    (5, "float", "z", 4, false),
    (4, "Vector3f", "m_Max", 12, false),
    (5, "float", "x", 4, false),
    (5, "float", "y", 4, false),
    (5, "float", "z", 4, false),
    (1, "VariableBoneCountWeights", "m_VariableBoneCountWeights", -1, false),
    (2, "vector", "m_Data", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "unsigned int", "data", 4, false),
    (1, "UInt8", "m_MeshCompression", 1, false),
    (1, "bool", "m_IsReadable", 1, false),
    (1, "bool", "m_KeepVertices", 1, false),
    (1, "bool", "m_KeepIndices", 1, true),
    (1, "int", "m_IndexFormat", 4, false),
    (1, "vector", "m_IndexBuffer", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "UInt8", "data", 1, false),
    (1, "VertexData", "m_VertexData", -1, true),
    (2, "unsigned int", "m_VertexCount", 4, false),
    (2, "vector", "m_Channels", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "ChannelInfo", "data", 4, false),
    (5, "UInt8", "stream", 1, false),
    (5, "UInt8", "offset", 1, false),
    (5, "UInt8", "format", 1, false),
    (5, "UInt8", "dimension", 1, false),
    (2, "TypelessData", "m_DataSize", -1, true),
    (3, "int", "size", 4, false),
    (3, "UInt8", "data", 1, false),
    (1, "CompressedMesh", "m_CompressedMesh", -1, false),
    (2, "PackedBitVector", "m_Vertices", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "float", "m_Range", 4, false),
    (3, "float", "m_Start", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_UV", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "float", "m_Range", 4, false),
    (3, "float", "m_Start", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_Normals", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "float", "m_Range", 4, false),
    (3, "float", "m_Start", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_Tangents", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "float", "m_Range", 4, false),
    (3, "float", "m_Start", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_Weights", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_NormalSigns", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_TangentSigns", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_FloatColors", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "float", "m_Range", 4, false),
    (3, "float", "m_Start", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_BoneIndices", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "PackedBitVector", "m_Triangles", -1, false),
    (3, "unsigned int", "m_NumItems", 4, false),
    (3, "vector", "m_Data", -1, false),
    (4, "Array", "Array", -1, true),
    (5, "int", "size", 4, false),
    (5, "UInt8", "data", 1, false),
    (3, "UInt8", "m_BitSize", 1, true),
    (2, "unsigned int", "m_UVInfo", 4, false),
    (1, "AABB", "m_LocalAABB", 24, false),
    (2, "Vector3f", "m_Center", 12, false),
    (3, "float", "x", 4, false),
    (3, "float", "y", 4, false),
    (3, "float", "z", 4, false),
    (2, "Vector3f", "m_Extent", 12, false),
    (3, "float", "x", 4, false),
    (3, "float", "y", 4, false),
    (3, "float", "z", 4, false),
    (1, "int", "m_MeshUsageFlags", 4, false),
    (1, "int", "m_CookingOptions", 4, false),
    (1, "vector", "m_BakedConvexCollisionMesh", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "UInt8", "data", 1, false),
    (1, "vector", "m_BakedTriangleCollisionMesh", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "UInt8", "data", 1, false),
    (1, "float", "m_MeshMetrics[0]", 4, false),
    (1, "float", "m_MeshMetrics[1]", 4, true),
    (1, "StreamingInfo", "m_StreamData", -1, false),
    (2, "UInt64", "offset", 8, false),
    (2, "unsigned int", "size", 4, false),
    (2, "string", "path", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "char", "data", 1, false),
    (1, "MeshLodInfo", "m_MeshLodInfo", -1, false),
    (2, "LodSelectionCurve", "m_LodSelectionCurve", 8, false),
    (3, "float", "m_LodSlope", 4, false),
    (3, "float", "m_LodBias", 4, true),
    (2, "int", "m_NumLevels", 4, true),
    (2, "vector", "m_SubMeshes", -1, true),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "MeshLodSubMesh", "data", -1, false),
    (5, "vector", "m_Levels", -1, true),
    (6, "Array", "Array", -1, true),
    (7, "int", "size", 4, false),
    (7, "MeshLodRange", "data", 8, false),
    (8, "unsigned int", "m_IndexStart", 4, false),
    (8, "unsigned int", "m_IndexCount", 4, true),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(count: u32, bits: u8, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(count.to_le_bytes());
        b.extend(1f32.to_le_bytes());
        b.extend(0f32.to_le_bytes());
        b.extend((data.len() as i32).to_le_bytes());
        b.extend(data);
        while b.len() % 4 != 0 {
            b.push(0);
        }
        b.push(bits);
        b
    }

    #[test]
    fn packed_floats_must_fit_their_data() {
        let unpack = |b: Vec<u8>| unpack_floats(&mut Reader::new(&b));
        assert_eq!(unpack(packed(2, 8, &[0, 255])).unwrap(), [0.0, 1.0]);
        assert_eq!(unpack(packed(0, 0, &[])).unwrap(), Vec::<f32>::new());
        // A corrupt count must not allocate for it.
        assert!(unpack(packed(u32::MAX, 32, &[0; 4])).is_err());
        assert!(unpack(packed(3, 0, &[])).is_err());
        assert!(unpack(packed(1, 33, &[0; 8])).is_err());
    }
}

// Unity 6000.3 release type-tree field descriptors; validated against installed game data.
const TERRAIN_COLLIDER_6000_3: &[(u8, &str, &str, i32, bool)] = &[
    (1, "PPtr<GameObject>", "m_GameObject", 12, false),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "PPtr<PhysicsMaterial>", "m_Material", 12, false),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "BitField", "m_IncludeLayers", 4, false),
    (2, "unsigned int", "m_Bits", 4, false),
    (1, "BitField", "m_ExcludeLayers", 4, false),
    (2, "unsigned int", "m_Bits", 4, false),
    (1, "int", "m_LayerOverridePriority", 4, false),
    (1, "bool", "m_ProvidesContacts", 1, true),
    (1, "bool", "m_Enabled", 1, true),
    (1, "PPtr<TerrainData>", "m_TerrainData", 12, false),
    (2, "int", "m_FileID", 4, false),
    (2, "SInt64", "m_PathID", 8, false),
    (1, "bool", "m_EnableTreeColliders", 1, false),
];

// Unity 6000.3 release type-tree field descriptors; validated against installed game data.
const TERRAIN_DATA_6000_3: &[(u8, &str, &str, i32, bool)] = &[
    (1, "string", "m_Name", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "char", "data", 1, false),
    (1, "SplatDatabase", "m_SplatDatabase", -1, false),
    (2, "vector", "m_TerrainLayers", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "PPtr<TerrainLayer>", "data", 12, false),
    (5, "int", "m_FileID", 4, false),
    (5, "SInt64", "m_PathID", 8, false),
    (2, "vector", "m_AlphaTextures", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "PPtr<Texture2D>", "data", 12, false),
    (5, "int", "m_FileID", 4, false),
    (5, "SInt64", "m_PathID", 8, false),
    (2, "int", "m_AlphamapResolution", 4, false),
    (2, "int", "m_BaseMapResolution", 4, false),
    (1, "DetailDatabase", "m_DetailDatabase", -1, false),
    (2, "vector", "m_Patches", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "DetailPatch", "data", -1, false),
    (5, "vector", "layerIndices", -1, false),
    (6, "Array", "Array", -1, true),
    (7, "int", "size", 4, false),
    (7, "UInt8", "data", 1, false),
    (5, "vector", "coverage", -1, false),
    (6, "Array", "Array", -1, true),
    (7, "int", "size", 4, false),
    (7, "UInt8", "data", 1, false),
    (2, "vector", "m_DetailPrototypes", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "DetailPrototype", "data", 116, false),
    (5, "PPtr<GameObject>", "prototype", 12, false),
    (6, "int", "m_FileID", 4, false),
    (6, "SInt64", "m_PathID", 8, false),
    (5, "PPtr<Texture2D>", "prototypeTexture", 12, false),
    (6, "int", "m_FileID", 4, false),
    (6, "SInt64", "m_PathID", 8, false),
    (5, "float", "minWidth", 4, false),
    (5, "float", "maxWidth", 4, false),
    (5, "float", "minHeight", 4, false),
    (5, "float", "maxHeight", 4, false),
    (5, "int", "noiseSeed", 4, false),
    (5, "float", "noiseSpread", 4, false),
    (5, "float", "holeTestRadius", 4, false),
    (5, "float", "density", 4, false),
    (5, "ColorRGBA", "healthyColor", 16, false),
    (6, "float", "r", 4, false),
    (6, "float", "g", 4, false),
    (6, "float", "b", 4, false),
    (6, "float", "a", 4, false),
    (5, "ColorRGBA", "dryColor", 16, false),
    (6, "float", "r", 4, false),
    (6, "float", "g", 4, false),
    (6, "float", "b", 4, false),
    (6, "float", "a", 4, false),
    (5, "int", "renderMode", 4, false),
    (5, "int", "usePrototypeMesh", 4, false),
    (5, "int", "useInstancing", 4, false),
    (5, "int", "useDensityScaling", 4, false),
    (5, "float", "alignToGround", 4, false),
    (5, "float", "positionJitter", 4, false),
    (5, "float", "targetCoverage", 4, false),
    (2, "int", "m_PatchCount", 4, false),
    (2, "int", "m_PatchSamples", 4, false),
    (2, "ColorRGBA", "WavingGrassTint", 16, false),
    (3, "float", "r", 4, false),
    (3, "float", "g", 4, false),
    (3, "float", "b", 4, false),
    (3, "float", "a", 4, false),
    (2, "float", "m_WavingGrassStrength", 4, false),
    (2, "float", "m_WavingGrassAmount", 4, false),
    (2, "float", "m_WavingGrassSpeed", 4, false),
    (2, "int", "m_DetailScatterMode", 4, false),
    (2, "vector", "m_TreeInstances", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "TreeInstance", "data", 36, false),
    (5, "Vector3f", "position", 12, false),
    (6, "float", "x", 4, false),
    (6, "float", "y", 4, false),
    (6, "float", "z", 4, false),
    (5, "float", "widthScale", 4, false),
    (5, "float", "heightScale", 4, false),
    (5, "float", "rotation", 4, false),
    (5, "ColorRGBA", "color", 4, false),
    (6, "unsigned int", "rgba", 4, false),
    (5, "ColorRGBA", "lightmapColor", 4, false),
    (6, "unsigned int", "rgba", 4, false),
    (5, "int", "index", 4, false),
    (2, "vector", "m_TreePrototypes", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "TreePrototype", "data", 20, false),
    (5, "PPtr<GameObject>", "prefab", 12, false),
    (6, "int", "m_FileID", 4, false),
    (6, "SInt64", "m_PathID", 8, false),
    (5, "float", "bendFactor", 4, false),
    (5, "int", "navMeshLod", 4, false),
    (2, "vector", "m_PreloadTextureAtlasData", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "PPtr<Texture2D>", "data", 12, false),
    (5, "int", "m_FileID", 4, false),
    (5, "SInt64", "m_PathID", 8, false),
    (2, "PPtr<Shader>", "m_DefaultShaders[0]", 12, false),
    (3, "int", "m_FileID", 4, false),
    (3, "SInt64", "m_PathID", 8, false),
    (2, "PPtr<Shader>", "m_DefaultShaders[1]", 12, false),
    (3, "int", "m_FileID", 4, false),
    (3, "SInt64", "m_PathID", 8, false),
    (2, "PPtr<Shader>", "m_DefaultShaders[2]", 12, false),
    (3, "int", "m_FileID", 4, false),
    (3, "SInt64", "m_PathID", 8, false),
    (1, "Heightmap", "m_Heightmap", -1, false),
    (2, "vector", "m_Heights", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "SInt16", "data", 2, false),
    (2, "vector", "m_Holes", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "UInt8", "data", 1, false),
    (2, "vector", "m_HolesLOD", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "UInt8", "data", 1, false),
    (2, "bool", "m_EnableHolesTextureCompression", 1, true),
    (2, "vector", "m_PrecomputedError", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "float", "data", 4, false),
    (2, "vector", "m_MinMaxPatchHeights", -1, false),
    (3, "Array", "Array", -1, true),
    (4, "int", "size", 4, false),
    (4, "float", "data", 4, false),
    (2, "int", "m_Resolution", 4, false),
    (2, "int", "m_Levels", 4, false),
    (2, "Vector3f", "m_Scale", 12, false),
    (3, "float", "x", 4, false),
    (3, "float", "y", 4, false),
    (3, "float", "z", 4, false),
    (1, "vector", "m_PreloadShaders", -1, false),
    (2, "Array", "Array", -1, true),
    (3, "int", "size", 4, false),
    (3, "PPtr<Shader>", "data", 12, false),
    (4, "int", "m_FileID", 4, false),
    (4, "SInt64", "m_PathID", 8, false),
];

pub fn terrain_collider(version: &str, data: &[u8]) -> Result<(bool, super::unity::PPtr), String> {
    let rows = layout_for(version, &[("6000.3", TERRAIN_COLLIDER_6000_3)], "TerrainCollider")?;
    let fields = fields(rows, data, true)?;
    let at = |name| {
        fields
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, r)| r.start)
            .ok_or_else(|| format!("TerrainCollider.{name}"))
    };
    let enabled = Reader::new(&data[at("m_Enabled")?..]).bool().ok_or("TerrainCollider.enabled")?;
    let pointer = Reader::new(&data[at("m_TerrainData")?..]).pptr().ok_or("TerrainCollider.terrainData")?;
    Ok((enabled, pointer))
}

/// Signed terrain samples scale by 32766 per heightmapScale.y, including negative elevations.
/// Bounds cover each
/// patch's highest vertex, hence every bilinear surface sample between them as well.
/// Normalization: UnityCsReference/Modules/Terrain/Public/PaintContext.cs, kNormalizedHeightScale.
pub fn terrain_patches(version: &str, data: &[u8]) -> Result<Vec<crate::geometry::navigation::Bounds>, String> {
    let rows = layout_for(version, &[("6000.3", TERRAIN_DATA_6000_3)], "TerrainData")?;
    fields(rows, data, true)?;
    let nodes = build(rows, &mut 0, 1);
    let at = |name| {
        locate(&nodes, &mut Reader::new(data), &["m_Heightmap", name])
            .map(|r| r.start)
            .ok_or_else(|| format!("TerrainData.{name}"))
    };
    let res = Reader::new(&data[at("m_Resolution")?..]).i32().ok_or("terrain resolution")?;
    let scale = Reader::new(&data[at("m_Scale")?..]).vec3().ok_or("terrain scale")?;
    let mut r = Reader::new(&data[at("m_Heights")?..]);
    let count = r.count().ok_or("terrain samples")?;
    if !(2..=8193).contains(&res) || count != res as usize * res as usize || !scale.iter().all(|v| v.is_finite() && *v > 0.0) {
        return Err("invalid terrain resolution, height count or scale".into());
    }
    let heights = (0..count).map(|_| r.i16()).collect::<Option<Vec<_>>>().ok_or("truncated terrain heights")?;
    height_patches(res as usize, scale, &heights)
}

fn height_patches(res: usize, scale: [f32; 3], heights: &[i16]) -> Result<Vec<crate::geometry::navigation::Bounds>, String> {
    let mut out = Vec::new();
    for z in (0..res - 1).step_by(16) {
        for x in (0..res - 1).step_by(16) {
            let (x1, z1) = ((x + 16).min(res - 1), (z + 16).min(res - 1));
            let max = (z..=z1).flat_map(|z| (x..=x1).map(move |x| heights[z * res + x])).max().unwrap();
            let top = (f32::from(max) + 1.0) / 32766.0 * scale[1] + 0.1;
            out.push(crate::geometry::navigation::Bounds {
                min: [x as f32 * scale[0], -10000.0, z as f32 * scale[2]],
                max: [x1 as f32 * scale[0], top, z1 as f32 * scale[2]],
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod navigation_tests {
    use super::*;
    #[test]
    fn height_patches_cover_peaks_at_shared_edges_and_the_last_cell() {
        let mut heights = vec![0; 18 * 18];
        heights[16 * 18 + 16] = 32766;
        heights[17 * 18 + 17] = 16383;
        let b = height_patches(18, [2.0, 600.0, 3.0], &heights).unwrap();
        assert_eq!(b.len(), 4);
        assert!(b.iter().all(|b| b.max[1] > 600.0));
        assert_eq!(b[3].max[0], 34.0);
        assert_eq!(b[3].max[2], 51.0);
        heights.fill(-16383);
        let below = height_patches(18, [2.0, 600.0, 3.0], &heights).unwrap();
        assert!(below.iter().all(|b| b.max[1] > -300.0 && b.max[1] < -299.0));
    }
}
