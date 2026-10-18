//! Unity's serialized files as a player build ships them (format 22, Unity 6, no type trees):
//! each file's object table and external references, a little-endian reader with Unity's
//! 4-byte alignment, and the built-in classes the game tables need.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A file mapped read-only into memory (the data folder is several gigabytes; only what is
/// read is paged in).
struct Map {
    ptr: *const u8,
    len: usize,
}

impl Map {
    fn open(path: &Path) -> std::io::Result<Map> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(Map { ptr: std::ptr::NonNull::dangling().as_ptr(), len: 0 });
        }
        // SAFETY: a private read-only mapping of a file we keep no writable handle to.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Map { ptr: ptr as *const u8, len })
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping lives as long as self and is never written.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: unmapping what open mapped, once.
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

/// Little-endian reads as Unity serializes: strings and arrays are an int32 count, then the
/// items, then padding to 4 bytes.
pub struct Reader<'a> {
    pub b: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Reader<'a> {
        Reader { b, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }
    pub fn align(&mut self) {
        self.pos = (self.pos + 3) & !3;
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }
    pub fn i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    pub fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.take(n)
    }
    /// An array's item count (an int32, sanity-checked against what is left).
    pub fn count(&mut self) -> Option<usize> {
        let n = self.i32()?;
        (n >= 0 && (n as usize) <= self.b.len().saturating_sub(self.pos)).then_some(n as usize)
    }
    pub fn string(&mut self) -> Option<String> {
        let n = self.count()?;
        let s = String::from_utf8_lossy(self.take(n)?).into_owned();
        self.align();
        Some(s)
    }
    /// A null-terminated string (file headers).
    fn cstring(&mut self) -> Option<String> {
        let end = self.b[self.pos..].iter().position(|c| *c == 0)?;
        let s = String::from_utf8_lossy(&self.b[self.pos..self.pos + end]).into_owned();
        self.pos += end + 1;
        Some(s)
    }
    pub fn pptr(&mut self) -> Option<PPtr> {
        Some(PPtr { file: self.i32()?, path: self.i64()? })
    }
    pub fn vec3(&mut self) -> Option<[f32; 3]> {
        Some([self.f32()?, self.f32()?, self.f32()?])
    }
    pub fn quat(&mut self) -> Option<[f32; 4]> {
        Some([self.f32()?, self.f32()?, self.f32()?, self.f32()?])
    }
}

/// A reference to an object: 0 is this file, n the n-th external file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PPtr {
    pub file: i32,
    pub path: i64,
}

impl PPtr {
    pub fn is_null(&self) -> bool {
        self.path == 0
    }
}

pub struct Object {
    pub path: i64,
    pub start: usize,
    pub size: usize,
    pub class_id: i32,
}

pub struct File {
    pub name: String,
    map: Map,
    pub unity_version: String,
    pub objects: Vec<Object>,
    by_path: HashMap<i64, usize>,
    /// External files, by their file name (lowercased), in PPtr order (file 1 is the first).
    pub externals: Vec<String>,
}

impl File {
    pub fn open(path: &Path) -> std::io::Result<File> {
        let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}: {what}", path.display()));
        let map = Map::open(path)?;
        let b = map.bytes();
        if b.len() < 48 {
            return Err(bad("too short for a serialized file"));
        }
        let be32 = |at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());
        let be64 = |at: usize| u64::from_be_bytes(b[at..at + 8].try_into().unwrap());
        let version = be32(8);
        if version < 22 || b[16] != 0 {
            return Err(bad(&format!("serialized file format {version} or big-endian (only 22+, little-endian, are read)")));
        }
        let data_offset = be64(32) as usize;
        let mut r = Reader { b, pos: 48 };
        let header = (|| {
            let unity_version = r.cstring()?;
            let _platform = r.i32()?;
            if r.bool()? {
                return None;
            }
            let mut types = Vec::new();
            for _ in 0..r.count()? {
                let class_id = r.i32()?;
                let _stripped = r.bool()?;
                let _script_index = r.i16()?;
                if class_id == 114 {
                    r.bytes(16)?;
                }
                r.bytes(16)?;
                types.push(class_id);
            }
            let mut objects = Vec::new();
            for _ in 0..r.count()? {
                r.align();
                let path = r.i64()?;
                let start = data_offset + r.u64()? as usize;
                let size = r.u32()? as usize;
                let class_id = *types.get(r.i32()? as usize)?;
                objects.push(Object { path, start, size, class_id });
            }
            for _ in 0..r.count()? {
                r.i32()?;
                r.align();
                r.i64()?;
            }
            let mut externals = Vec::new();
            for _ in 0..r.count()? {
                r.cstring()?;
                r.bytes(16)?;
                r.i32()?;
                let p = r.cstring()?;
                externals.push(p.rsplit('/').next().unwrap_or(&p).to_ascii_lowercase());
            }
            Some((unity_version, objects, externals))
        })();
        let Some((unity_version, objects, externals)) = header else { return Err(bad("unreadable header (type trees in the file?)")) };
        if objects.iter().any(|o| o.start + o.size > b.len()) {
            return Err(bad("an object lies past the end of the file"));
        }
        let by_path = objects.iter().enumerate().map(|(i, o)| (o.path, i)).collect();
        let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        Ok(File { name, map, unity_version, objects, by_path, externals })
    }

    pub fn object(&self, path: i64) -> Option<&Object> {
        self.objects.get(*self.by_path.get(&path)?)
    }

    pub fn data(&self, o: &Object) -> &[u8] {
        &self.map.bytes()[o.start..o.start + o.size]
    }
}

/// An object anywhere in the data folder: (file index, path id).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub usize, pub i64);

/// The game's data folder ("Planet Crafter_Data"): its serialized files, opened as references
/// reach them.
pub struct Data {
    pub dir: PathBuf,
    pub files: Vec<File>,
    by_name: HashMap<String, usize>,
}

impl Data {
    /// The build's scene paths by build index (scene N is the file levelN).
    pub fn build_scenes(&mut self) -> Vec<String> {
        let Ok(ggm) = self.file("globalgamemanagers") else { return Vec::new() };
        let settings = self.files[ggm].objects.iter().find(|o| o.class_id == BUILD_SETTINGS).map(|o| o.path);
        settings.and_then(|p| build_scenes(&mut self.reader(Id(ggm, p))?)).unwrap_or_default()
    }

    pub fn open(dir: &Path) -> std::io::Result<Data> {
        let mut data = Data { dir: dir.to_path_buf(), files: Vec::new(), by_name: HashMap::new() };
        data.file("globalgamemanagers")?;
        Ok(data)
    }

    /// The index of a file of the folder by name (case-insensitive), opening it the first time.
    pub fn file(&mut self, name: &str) -> std::io::Result<usize> {
        let key = name.to_ascii_lowercase();
        if let Some(i) = self.by_name.get(&key) {
            return Ok(*i);
        }
        let entry = std::fs::read_dir(&self.dir)?.flatten().find(|e| e.file_name().to_string_lossy().to_ascii_lowercase() == key);
        let path = entry.map(|e| e.path()).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, format!("{}: no {name}", self.dir.display())))?;
        let f = File::open(&path)?;
        self.files.push(f);
        self.by_name.insert(key, self.files.len() - 1);
        Ok(self.files.len() - 1)
    }

    /// Where a reference from a file points; None for null and for Unity's built-in resources.
    pub fn resolve(&mut self, from: usize, p: PPtr) -> Option<Id> {
        if p.is_null() {
            return None;
        }
        if p.file == 0 {
            return Some(Id(from, p.path));
        }
        let name = self.files[from].externals.get(p.file as usize - 1)?.clone();
        if name.contains("unity default resources") || name.contains("unity_builtin_extra") {
            return None;
        }
        Some(Id(self.file(&name).ok()?, p.path))
    }

    /// Where a reference points, Unity's built-in resources (Resources/unity default resources,
    /// unity_builtin_extra) included.
    pub fn resolve_builtin(&mut self, from: usize, p: PPtr) -> Option<Id> {
        if p.is_null() || p.file == 0 {
            return self.resolve(from, p);
        }
        let name = self.files[from].externals.get(p.file as usize - 1)?.clone();
        let builtin = ["unity default resources", "unity_builtin_extra"].into_iter().find(|b| name.contains(b));
        let Some(builtin) = builtin else { return self.resolve(from, p) };
        let key = format!("resources/{builtin}");
        let file = match self.by_name.get(&key) {
            Some(i) => *i,
            None => {
                let f = File::open(&self.dir.join("Resources").join(builtin)).ok()?;
                self.files.push(f);
                self.by_name.insert(key, self.files.len() - 1);
                self.files.len() - 1
            }
        };
        Some(Id(file, p.path))
    }

    pub fn class_of(&self, id: Id) -> Option<i32> {
        Some(self.files[id.0].object(id.1)?.class_id)
    }

    /// A reader at the object's data.
    pub fn reader(&self, id: Id) -> Option<Reader<'_>> {
        let f = &self.files[id.0];
        Some(Reader::new(f.data(f.object(id.1)?)))
    }
}

pub const GAME_OBJECT: i32 = 1;
pub const TRANSFORM: i32 = 4;
pub const MESH_FILTER: i32 = 33;
pub const MESH: i32 = 43;
pub const RIGIDBODY: i32 = 54;
pub const MESH_COLLIDER: i32 = 64;
pub const BOX_COLLIDER: i32 = 65;
pub const MONO_BEHAVIOUR: i32 = 114;
pub const MONO_SCRIPT: i32 = 115;
pub const SPHERE_COLLIDER: i32 = 135;
pub const CAPSULE_COLLIDER: i32 = 136;
pub const BUILD_SETTINGS: i32 = 141;
pub const RESOURCE_MANAGER: i32 = 147;
pub const RECT_TRANSFORM: i32 = 224;

pub struct GameObject {
    pub components: Vec<PPtr>,
    pub layer: u32,
    pub name: String,
    pub tag: u16,
    pub active: bool,
}

pub fn game_object(r: &mut Reader) -> Option<GameObject> {
    let mut components = Vec::new();
    for _ in 0..r.count()? {
        components.push(r.pptr()?);
    }
    Some(GameObject { components, layer: r.u32()?, name: r.string()?, tag: r.u16()?, active: r.bool()? })
}

/// Transform and RectTransform (which starts the same way).
pub struct Transform {
    pub game_object: PPtr,
    pub rotation: [f32; 4],
    pub position: [f32; 3],
    pub scale: [f32; 3],
    pub children: Vec<PPtr>,
    pub father: PPtr,
}

pub fn transform(r: &mut Reader) -> Option<Transform> {
    let game_object = r.pptr()?;
    let (rotation, position, scale) = (r.quat()?, r.vec3()?, r.vec3()?);
    let mut children = Vec::new();
    for _ in 0..r.count()? {
        children.push(r.pptr()?);
    }
    Some(Transform { game_object, rotation, position, scale, children, father: r.pptr()? })
}

/// What every MonoBehaviour starts with; its script's fields follow.
pub struct Behaviour {
    pub game_object: PPtr,
    pub enabled: bool,
    pub script: PPtr,
    pub name: String,
}

pub fn behaviour(r: &mut Reader) -> Option<Behaviour> {
    let game_object = r.pptr()?;
    let enabled = r.bool()?;
    r.align();
    Some(Behaviour { game_object, enabled, script: r.pptr()?, name: r.string()? })
}

pub struct Script {
    pub name: String,
    pub class: String,
    pub namespace: String,
    pub assembly: String,
}

pub fn mono_script(r: &mut Reader) -> Option<Script> {
    let name = r.string()?;
    r.i32()?;
    r.bytes(16)?;
    Some(Script { name, class: r.string()?, namespace: r.string()?, assembly: r.string()? })
}

/// What colliders start with: (game object, isTrigger, enabled); the shape follows.
pub fn collider(r: &mut Reader) -> Option<(PPtr, bool, bool)> {
    let game_object = r.pptr()?;
    r.pptr()?;
    r.u32()?;
    r.u32()?;
    r.i32()?;
    let trigger = r.bool()?;
    r.bool()?;
    r.align();
    let enabled = r.bool()?;
    r.align();
    Some((game_object, trigger, enabled))
}

pub enum Shape {
    Box { center: [f32; 3], size: [f32; 3] },
    Sphere { center: [f32; 3], radius: f32 },
    Capsule { center: [f32; 3], radius: f32, height: f32, direction: i32 },
    Mesh,
}

pub struct Collider {
    pub game_object: PPtr,
    pub trigger: bool,
    pub enabled: bool,
    pub shape: Shape,
}

pub fn read_collider(class_id: i32, r: &mut Reader) -> Option<Collider> {
    let (game_object, trigger, enabled) = collider(r)?;
    let shape = match class_id {
        BOX_COLLIDER => {
            let size = r.vec3()?;
            Shape::Box { center: r.vec3()?, size }
        }
        SPHERE_COLLIDER => {
            let radius = r.f32()?;
            Shape::Sphere { center: r.vec3()?, radius }
        }
        CAPSULE_COLLIDER => {
            let (radius, height, direction) = (r.f32()?, r.f32()?, r.i32()?);
            Shape::Capsule { center: r.vec3()?, radius, height, direction }
        }
        _ => Shape::Mesh,
    };
    Some(Collider { game_object, trigger, enabled, shape })
}

/// BuildSettings: the scenes in build order (level<i> is scene i).
pub fn build_scenes(r: &mut Reader) -> Option<Vec<String>> {
    let mut scenes = Vec::new();
    for _ in 0..r.count()? {
        scenes.push(r.string()?);
    }
    Some(scenes)
}

/// ResourceManager: Resources.Load paths -> objects.
pub fn resources(r: &mut Reader) -> Option<Vec<(String, PPtr)>> {
    let mut out = Vec::new();
    for _ in 0..r.count()? {
        out.push((r.string()?, r.pptr()?));
    }
    Some(out)
}

/// Unity's built-in class names by class id (what Object.GetType().Name gives for them).
pub fn class_name(id: i32) -> Option<&'static str> {
    Some(match id {
        1 => "GameObject",
        2 => "Component",
        3 => "LevelGameManager",
        4 => "Transform",
        5 => "TimeManager",
        6 => "GlobalGameManager",
        8 => "Behaviour",
        9 => "GameManager",
        11 => "AudioManager",
        12 => "ParticleAnimator",
        13 => "InputManager",
        15 => "EllipsoidParticleEmitter",
        17 => "Pipeline",
        18 => "EditorExtension",
        19 => "Physics2DSettings",
        20 => "Camera",
        21 => "Material",
        23 => "MeshRenderer",
        25 => "Renderer",
        26 => "ParticleRenderer",
        27 => "Texture",
        28 => "Texture2D",
        29 => "OcclusionCullingSettings",
        30 => "GraphicsSettings",
        33 => "MeshFilter",
        41 => "OcclusionPortal",
        43 => "Mesh",
        45 => "Skybox",
        47 => "QualitySettings",
        48 => "Shader",
        49 => "TextAsset",
        50 => "Rigidbody2D",
        51 => "Physics2DManager",
        53 => "Collider2D",
        54 => "Rigidbody",
        55 => "PhysicsManager",
        56 => "Collider",
        57 => "Joint",
        58 => "CircleCollider2D",
        59 => "HingeJoint",
        60 => "PolygonCollider2D",
        61 => "BoxCollider2D",
        62 => "PhysicsMaterial2D",
        64 => "MeshCollider",
        65 => "BoxCollider",
        66 => "CompositeCollider2D",
        68 => "EdgeCollider2D",
        70 => "CapsuleCollider2D",
        72 => "ComputeShader",
        74 => "AnimationClip",
        75 => "ConstantForce",
        76 => "WorldParticleCollider",
        78 => "TagManager",
        81 => "AudioListener",
        82 => "AudioSource",
        83 => "AudioClip",
        84 => "RenderTexture",
        86 => "CustomRenderTexture",
        87 => "MeshParticleEmitter",
        88 => "ParticleEmitter",
        89 => "Cubemap",
        90 => "Avatar",
        91 => "AnimatorController",
        92 => "GUILayer",
        93 => "RuntimeAnimatorController",
        94 => "ScriptMapper",
        95 => "Animator",
        96 => "TrailRenderer",
        98 => "DelayedCallManager",
        102 => "TextMesh",
        104 => "RenderSettings",
        108 => "Light",
        109 => "CGProgram",
        110 => "BaseAnimationTrack",
        111 => "Animation",
        114 => "MonoBehaviour",
        115 => "MonoScript",
        116 => "MonoManager",
        117 => "Texture3D",
        118 => "NewAnimationTrack",
        119 => "Projector",
        120 => "LineRenderer",
        121 => "Flare",
        122 => "Halo",
        123 => "LensFlare",
        124 => "FlareLayer",
        125 => "HaloLayer",
        126 => "NavMeshProjectSettings",
        127 => "HaloManager",
        128 => "Font",
        129 => "PlayerSettings",
        130 => "NamedObject",
        131 => "GUITexture",
        132 => "GUIText",
        133 => "GUIElement",
        134 => "PhysicMaterial",
        135 => "SphereCollider",
        136 => "CapsuleCollider",
        137 => "SkinnedMeshRenderer",
        138 => "FixedJoint",
        140 => "RaycastCollider",
        141 => "BuildSettings",
        142 => "AssetBundle",
        143 => "CharacterController",
        144 => "CharacterJoint",
        145 => "SpringJoint",
        146 => "WheelCollider",
        147 => "ResourceManager",
        148 => "NetworkView",
        149 => "NetworkManager",
        150 => "PreloadData",
        152 => "MovieTexture",
        153 => "ConfigurableJoint",
        154 => "TerrainCollider",
        155 => "MasterServerInterface",
        156 => "TerrainData",
        157 => "LightmapSettings",
        158 => "WebCamTexture",
        159 => "EditorSettings",
        160 => "InteractiveCloth",
        161 => "ClothRenderer",
        162 => "EditorUserSettings",
        163 => "SkinnedCloth",
        164 => "AudioReverbFilter",
        165 => "AudioHighPassFilter",
        166 => "AudioChorusFilter",
        167 => "AudioReverbZone",
        168 => "AudioEchoFilter",
        169 => "AudioLowPassFilter",
        170 => "AudioDistortionFilter",
        171 => "SparseTexture",
        180 => "AudioBehaviour",
        181 => "AudioFilter",
        182 => "WindZone",
        183 => "Cloth",
        184 => "SubstanceArchive",
        185 => "ProceduralMaterial",
        186 => "ProceduralTexture",
        187 => "Texture2DArray",
        188 => "CubemapArray",
        191 => "OffMeshLink",
        192 => "OcclusionArea",
        193 => "Tree",
        194 => "NavMeshObsolete",
        195 => "NavMeshAgent",
        196 => "NavMeshSettings",
        197 => "LightProbesLegacy",
        198 => "ParticleSystem",
        199 => "ParticleSystemRenderer",
        200 => "ShaderVariantCollection",
        205 => "LODGroup",
        206 => "BlendTree",
        207 => "Motion",
        208 => "NavMeshObstacle",
        210 => "SortingGroup",
        212 => "SpriteRenderer",
        213 => "Sprite",
        214 => "CachedSpriteAtlas",
        215 => "ReflectionProbe",
        216 => "ReflectionProbes",
        218 => "Terrain",
        220 => "LightProbeGroup",
        221 => "AnimatorOverrideController",
        222 => "CanvasRenderer",
        223 => "Canvas",
        224 => "RectTransform",
        225 => "CanvasGroup",
        226 => "BillboardAsset",
        227 => "BillboardRenderer",
        228 => "SpeedTreeWindAsset",
        229 => "AnchoredJoint2D",
        230 => "Joint2D",
        231 => "SpringJoint2D",
        232 => "DistanceJoint2D",
        233 => "HingeJoint2D",
        234 => "SliderJoint2D",
        235 => "WheelJoint2D",
        236 => "ClusterInputManager",
        237 => "BaseVideoTexture",
        238 => "NavMeshData",
        240 => "AudioMixer",
        241 => "AudioMixerController",
        243 => "AudioMixerGroupController",
        244 => "AudioMixerEffectController",
        245 => "AudioMixerSnapshotController",
        246 => "PhysicsUpdateBehaviour2D",
        247 => "ConstantForce2D",
        248 => "Effector2D",
        249 => "AreaEffector2D",
        250 => "PointEffector2D",
        251 => "PlatformEffector2D",
        252 => "SurfaceEffector2D",
        253 => "BuoyancyEffector2D",
        254 => "RelativeJoint2D",
        255 => "FixedJoint2D",
        256 => "FrictionJoint2D",
        257 => "TargetJoint2D",
        258 => "LightProbes",
        259 => "LightProbeProxyVolume",
        271 => "SampleClip",
        272 => "AudioMixerSnapshot",
        273 => "AudioMixerGroup",
        280 => "NScreenBridge",
        290 => "AssetBundleManifest",
        292 => "UnityAdsManager",
        300 => "RuntimeInitializeOnLoadManager",
        301 => "CloudWebServicesManager",
        303 => "UnityAnalyticsManager",
        304 => "CrashReportManager",
        305 => "PerformanceReportingManager",
        310 => "UnityConnectSettings",
        319 => "AvatarMask",
        320 => "PlayableDirector",
        328 => "VideoPlayer",
        329 => "VideoClip",
        330 => "ParticleSystemForceField",
        331 => "SpriteMask",
        362 => "WorldAnchor",
        363 => "OcclusionCullingData",
        1000 => "SmallestEditorClassID",
        1001 => "PrefabInstance",
        1002 => "EditorExtensionImpl",
        1003 => "AssetImporter",
        1004 => "AssetDatabaseV1",
        1005 => "Mesh3DSImporter",
        1006 => "TextureImporter",
        1007 => "ShaderImporter",
        1008 => "ComputeShaderImporter",
        1020 => "AudioImporter",
        1026 => "HierarchyState",
        1027 => "GUIDSerializer",
        1028 => "AssetMetaData",
        1029 => "DefaultAsset",
        1030 => "DefaultImporter",
        1031 => "TextScriptImporter",
        1032 => "SceneAsset",
        1034 => "NativeFormatImporter",
        1035 => "MonoImporter",
        1037 => "AssetServerCache",
        1038 => "LibraryAssetImporter",
        1040 => "ModelImporter",
        1041 => "FBXImporter",
        1042 => "TrueTypeFontImporter",
        1044 => "MovieImporter",
        1045 => "EditorBuildSettings",
        1046 => "DDSImporter",
        1048 => "InspectorExpandedState",
        1049 => "AnnotationManager",
        1050 => "PluginImporter",
        1051 => "EditorUserBuildSettings",
        1052 => "PVRImporter",
        1053 => "ASTCImporter",
        1054 => "KTXImporter",
        1055 => "IHVImageFormatImporter",
        1101 => "AnimatorStateTransition",
        1102 => "AnimatorState",
        1105 => "HumanTemplate",
        1107 => "AnimatorStateMachine",
        1108 => "PreviewAnimationClip",
        1109 => "AnimatorTransition",
        1110 => "SpeedTreeImporter",
        1111 => "AnimatorTransitionBase",
        1112 => "SubstanceImporter",
        1113 => "LightmapParameters",
        1120 => "LightingDataAsset",
        1121 => "GISRaster",
        1122 => "GISRasterImporter",
        1123 => "CadImporter",
        1124 => "SketchUpImporter",
        1125 => "BuildReport",
        1126 => "PackedAssets",
        1127 => "VideoClipImporter",
        _ => return None,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The installed game's data folder (PCDS_GAME_DIR or the default Steam library).
    pub fn game_data() -> Option<PathBuf> {
        let game = std::env::var("PCDS_GAME_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/Steam/steamapps/common/The Planet Crafter"));
        let d = game.join("Planet Crafter_Data");
        d.join("globalgamemanagers").exists().then_some(d)
    }

    #[test]
    fn the_build_lists_its_scenes_and_scripts() {
        let Some(dir) = game_data() else { return };
        let mut data = Data::open(&dir).unwrap();
        let ggm = data.file("globalgamemanagers").unwrap();
        let settings = data.files[ggm].objects.iter().find(|o| o.class_id == BUILD_SETTINGS).map(|o| o.path).unwrap();
        let scenes = build_scenes(&mut data.reader(Id(ggm, settings)).unwrap()).unwrap();
        assert!(scenes.len() > 100 && scenes[0].ends_with(".unity"), "{:?}", &scenes[..3]);
        let assets = data.file("globalgamemanagers.assets").unwrap();
        let scripts: Vec<Script> = data.files[assets].objects.iter().filter(|o| o.class_id == MONO_SCRIPT).filter_map(|o| mono_script(&mut Reader::new(data.files[assets].data(o)))).collect();
        assert!(scripts.iter().any(|s| s.namespace == "SpaceCraft" && s.class == "GroupDataItem" && s.assembly == "Assembly-CSharp"));
    }
}

/// Debug: SpaceCraft MonoBehaviours per file, by class (pcds-server native-scan DATA_DIR).
pub fn scan(args: &[String]) {
    let dir = Path::new(&args[0]);
    let mut data = Data::open(dir).expect("open data");
    let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| (n.starts_with("level") && !n.contains('.')) || n.ends_with(".assets")).collect();
    names.sort_by_key(|n| (n.len(), n.clone()));
    let mut scripts: HashMap<Id, String> = HashMap::new();
    for name in names {
        let Ok(f) = data.file(&name) else { continue };
        let mut counts: HashMap<String, usize> = HashMap::new();
        let behaviours: Vec<i64> = data.files[f].objects.iter().filter(|o| o.class_id == MONO_BEHAVIOUR).map(|o| o.path).collect();
        for path in behaviours {
            let Some(b) = data.reader(Id(f, path)).and_then(|mut r| behaviour(&mut r)) else { continue };
            let Some(sid) = data.resolve(f, b.script) else { continue };
            let class = scripts.entry(sid).or_insert_with(|| data.reader(sid).and_then(|mut r| mono_script(&mut r)).map(|s| format!("{}.{}", s.namespace, s.class)).unwrap_or_default()).clone();
            if class.starts_with("SpaceCraft.") {
                *counts.entry(class).or_default() += 1;
            }
        }
        if !counts.is_empty() {
            let mut c: Vec<_> = counts.into_iter().collect();
            c.sort_by(|a, b| b.1.cmp(&a.1));
            for (class, n) in &c { println!("{name}\t{class}\t{n}"); }
        }
    }
}
