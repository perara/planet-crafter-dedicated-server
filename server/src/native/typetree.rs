//! Reading a built-in class by its Unity type tree (depth, type, name, size, align rows), for
//! the one class whose layout is too long to write out by hand: PlayerSettings, for the game
//! version. Layouts change with Unity versions, so each is tied to one.

use super::unity::Reader;

struct Node<'a> {
    ty: &'a str,
    name: &'a str,
    size: i32,
    align: bool,
    children: Vec<Node<'a>>,
}

fn build<'a>(rows: &[(u8, &'a str, &'a str, i32, bool)], at: &mut usize, depth: u8) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    while *at < rows.len() && rows[*at].0 == depth {
        let (_, ty, name, size, align) = rows[*at];
        *at += 1;
        let children = build(rows, at, depth + 1);
        out.push(Node { ty, name, size, align, children });
    }
    out
}

/// Reads a node, returning the string value of `want` when the walk reaches it.
fn read(n: &Node, r: &mut Reader, want: &str) -> Result<Option<String>, ()> {
    if n.ty == "string" {
        let s = r.string().ok_or(())?;
        return Ok((n.name == want).then_some(s));
    }
    if let Some(array) = n.children.iter().find(|c| c.ty == "Array") {
        let count = r.count().ok_or(())?;
        let item = array.children.get(1).ok_or(())?;
        for _ in 0..count {
            read(item, r, want)?;
        }
        if array.align || n.align {
            r.align();
        }
        return Ok(None);
    }
    if n.children.is_empty() {
        r.bytes(usize::try_from(n.size).map_err(|_| ())?).ok_or(())?;
    } else {
        for c in &n.children {
            if let Some(found) = read(c, r, want)? {
                return Ok(Some(found));
            }
        }
    }
    if n.align {
        r.align();
    }
    Ok(None)
}

/// PlayerSettings.bundleVersion (Application.version), for the Unity versions with a layout.
pub fn bundle_version(unity_version: &str, data: &[u8]) -> Result<String, String> {
    let rows = match unity_version.split('.').take(2).collect::<Vec<_>>().as_slice() {
        ["6000", "3"] => PLAYER_SETTINGS_6000_3,
        _ => return Err(format!("no PlayerSettings layout for Unity {unity_version}: the game's version cannot be read (add one to native/typetree.rs)")),
    };
    let nodes = build(rows, &mut 0, 1);
    let mut r = Reader::new(data);
    for n in &nodes {
        match read(n, &mut r, "bundleVersion") {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {}
            Err(()) => return Err(format!("PlayerSettings did not read as Unity {unity_version}'s layout (at {})", n.name)),
        }
    }
    Err("PlayerSettings has no bundleVersion".into())
}

/// Unity 6000.3's PlayerSettings layout from its first field to bundleVersion, as (depth,
/// type, name, byte size, aligned after): the game version is bundleVersion.
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
