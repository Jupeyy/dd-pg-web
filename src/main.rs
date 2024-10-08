use anyhow::anyhow;
use axum::{
    async_trait, body::StreamBody, extract::Query, http::header, response::IntoResponse,
    routing::get, Router,
};
use base::system::{System, SystemTimeInterface};
use base_fs::filesys::FileSystem;
use base_http::http::HttpClient;
use base_io::io::Io;
use client_containers::{
    emoticons::{EmoticonsContainer, EMOTICONS_CONTAINER_PATH},
    entities::{EntitiesContainer, ENTITIES_CONTAINER_PATH},
    hooks::{HookContainer, HOOK_CONTAINER_PATH},
    skins::{SkinContainer, SKIN_CONTAINER_PATH},
    weapons::{WeaponContainer, WEAPON_CONTAINER_PATH},
};
use client_render::{
    emoticons::render::{RenderEmoticon, RenderEmoticonPipe},
    nameplates::render::{NameplateRender, NameplateRenderPipe},
};
use client_render_base::{
    map::{
        map_pipeline::MapPipeline,
        render_pipe::{Camera, GameTimeInfo, RenderPipeline},
    },
    render::{
        animation::AnimState,
        canvas_mapping::CanvasMappingIngame,
        default_anim::{base_anim, idle_anim, inair_anim},
        tee::{RenderTee, RenderTeeHandMath, TeeRenderHands, TeeRenderInfo, TeeRenderSkinColor},
        toolkit::ToolkitRender,
    },
};
use client_render_game::map::render_map_base::{ClientMapRender, RenderMapLoading};
use config::config::{ConfigBackend, ConfigDebug, ConfigGfx, ConfigSound};
use game_interface::types::{
    emoticons::{EmoticonType, IntoEnumIterator},
    network_string::NetworkString,
    render::character::{CharacterRenderInfo, TeeEye},
    resource_key::{NetworkResourceKey, ResourceKey},
    weapons::WeaponType,
};
use graphics::graphics::graphics::{Graphics, ScreenshotCb};
use graphics_backend::{
    backend::{
        GraphicsBackend, GraphicsBackendBase, GraphicsBackendIoLoading, GraphicsBackendLoading,
    },
    window::BackendWindow,
};

use graphics_backend_traits::traits::GraphicsBackendInterface;

use graphics_types::rendering::{ColorRgba, State};
use math::math::{
    normalize,
    vector::{dvec2, vec2},
};
use palette::convert::FromColorUnclamped;
use pool::datatypes::PoolLinkedHashMap;
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde::{Deserialize, Serialize};
use serenity::all::{
    Context, CreateAttachment, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage, EventHandler, GatewayIntents, GuildId, Interaction, Mention,
    Ready, StandardFramework,
};
use sound::sound::SoundManager;
use sound_backend::sound_backend::SoundBackend;
use std::{
    cell::RefCell,
    io::Cursor,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::sync::{
    oneshot::{self, Sender},
    Mutex,
};
use tokio_util::io::ReaderStream;
use ui_base::{
    font_data::{UiFontData, UiFontDataLoading},
    ui::UiCreator,
};
use urlencoding::encode;

pub struct ClientWrapper(Client);

unsafe impl Sync for ClientWrapper {}
unsafe impl Send for ClientWrapper {}

static CLIENT: Mutex<Option<ClientWrapper>> = Mutex::const_new(None);
static HTTP: LazyLock<Arc<reqwest::Client>> = LazyLock::new(Default::default);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Skin {
    #[serde(rename = "skin_name")]
    name: NetworkString<24>,
    #[serde(alias = "skin_color_body")]
    color_body: Option<i32>,
    #[serde(alias = "skin_color_feet")]
    color_feet: Option<i32>,
}

type PlayerApiState = Arc<parking_lot::Mutex<std::time::Instant>>;
static PLAYERS: LazyLock<PlayerApiState> = LazyLock::new(|| {
    Arc::new(parking_lot::Mutex::new(
        std::time::Instant::now() - Duration::from_secs(60 * 60 * 60),
    ))
});

#[derive(Debug, Default, Deserialize)]
struct RenderParams {
    /// Name of the skin to draw
    skin_name: NetworkString<24>,
    /// Optional player name to render as nameplate
    player_name: Option<NetworkString<128>>,
    /// Camera zoom
    zoom: Option<f32>,
    /// Camera pos x
    x: Option<f32>,
    /// Camera pos y
    y: Option<f32>,
    /// Legacy color body
    body: Option<i32>,
    /// Legacy color feet
    feet: Option<i32>,
    /// Cursor dir x
    dir_x: Option<f32>,
    /// Cursor dir y
    dir_y: Option<f32>,
    /// Tee eyes
    eyes: Option<String>,
    /// Tee weapon
    weapon: Option<String>,
    /// Tee emoticon
    emoticon: Option<String>,
    /// Whether the Tee used its double jump
    used_air_jump: Option<bool>,
    /// Whether the Tee is in the air right now
    in_air: Option<bool>,
    /// The x position of the hook relative to the Tee
    hook_x: Option<f32>,
    /// The y position of the hook relative to the Tee
    hook_y: Option<f32>,
    /// The current time of the rendering (e.g. for map animations) in ms.
    time: Option<u64>,

    /// Name of the map to render
    map_name: Option<String>,
    /// Use skins.tw player api to fetch latest
    /// skin of the player
    use_player_api: Option<bool>,
}

struct ClientLoad {
    backend_loading: GraphicsBackendLoading,
    backend_loading_io: GraphicsBackendIoLoading,
    sys: System,
    io: Io,
    tp: Arc<ThreadPool>,
}

fn config_gl() -> ConfigBackend {
    ConfigBackend {
        full_pipeline_creation: false,
        ..Default::default()
    }
}
fn config_gfx() -> ConfigGfx {
    ConfigGfx::default()
}
fn config_dbg() -> ConfigDebug {
    ConfigDebug::default()
}

struct Client {
    graphics_backend: Rc<GraphicsBackend>,
    graphics: Graphics,

    tee_renderer: RenderTee,
    nameplate_renderer: NameplateRender,
    emoticon_renderer: RenderEmoticon,
    toolkit_renderer: ToolkitRender,

    skin_container: SkinContainer,
    entities_container: EntitiesContainer,
    weapon_container: WeaponContainer,
    emoticon_container: EmoticonsContainer,
    hooks_container: HookContainer,

    sys: System,
    client_map: ClientMapRender,
}

impl Client {
    pub fn map_canvas_for_players(
        graphics: &Graphics,
        state: &mut State,
        center_x: f32,
        center_y: f32,
        zoom: f32,
    ) {
        CanvasMappingIngame::new(graphics)
            .map_canvas_for_ingame_items(state, center_x, center_y, zoom);
    }

    pub fn render(&mut self, params: RenderParams, sender: Sender<anyhow::Result<Vec<u8>>>) {
        let skin_name = params.skin_name;

        let map_name = params.map_name.unwrap_or("ctf1".to_string());
        let is_ctf1 = map_name == "ctf1";

        let default_x = if is_ctf1 { 173.12 } else { 1358.08 };
        let default_y = if is_ctf1 { 688.96 } else { 24240.96 };

        let mut zoom = params.zoom.unwrap_or(0.5);
        let mut x = params.x.unwrap_or(default_x);
        let mut y = params.y.unwrap_or(default_y);
        let mut dir_x = params.dir_x.unwrap_or(1.0);
        let mut dir_y = params.dir_y.unwrap_or(0.0);

        if zoom.is_nan() || zoom.is_infinite() {
            zoom = 1.0;
        }
        zoom = zoom.clamp(0.001, 20.0);

        if x.is_nan() || x.is_infinite() {
            x = 0.0;
        }
        x = x.clamp(0.0, 300000.0);

        if y.is_nan() || y.is_infinite() {
            y = 0.0;
        }
        y = y.clamp(0.0, 300000.0);

        if dir_x.is_nan() || dir_x.is_infinite() {
            dir_x = 0.0;
        }
        dir_x = dir_x.clamp(-1.0, 1.0);

        if dir_y.is_nan() || dir_y.is_infinite() {
            dir_y = 0.0;
        }
        dir_y = dir_y.clamp(-1.0, 1.0);

        let custom_color = params.body.is_some();

        let color_body = params.body.unwrap_or(0);
        let color_feet = params.feet.unwrap_or(0);

        if dir_x.abs() < 0.001 && dir_y.abs() < 0.001 {
            dir_x = 1.0;
        }

        let dir = normalize(&vec2::new(dir_x, dir_y));

        let hook_pos = params.hook_x.zip(params.hook_y).map(|(mut x, mut y)| {
            if x.is_nan() || x.is_infinite() {
                x = 0.0;
            }
            x = x.clamp(-10000.0, 10000.0);

            if y.is_nan() || y.is_infinite() {
                y = 0.0;
            }
            y = y.clamp(-10000.0, 10000.0);
            vec2::new(x, y)
        });

        let tee_eyes = match params
            .eyes
            .unwrap_or("normal".to_string())
            .to_lowercase()
            .as_str()
        {
            "normal" => TeeEye::Normal,
            "angry" => TeeEye::Angry,
            "pain" => TeeEye::Pain,
            "happy" => TeeEye::Happy,
            "surprised" => TeeEye::Surprised,
            "blink" => TeeEye::Blink,
            _ => TeeEye::Normal,
        };

        let weapon = params
            .weapon
            .map(|weapon| match weapon.to_lowercase().as_str() {
                "gun" => WeaponType::Gun,
                "shotgun" => WeaponType::Shotgun,
                "grenade" => WeaponType::Grenade,
                "laser" => WeaponType::Laser,
                _ => WeaponType::Hammer,
            });

        let emoticon = params.emoticon.and_then(|emoticon| {
            EmoticonType::iter().find(|e| {
                let e_str: &'static str = e.into();

                e_str.to_lowercase() == emoticon.to_lowercase()
            })
        });

        // at most 1 years
        let cur_time = Duration::from_millis(params.time.unwrap_or_default().clamp(0, 31536000000));

        let map_file = &mut self.client_map;
        let map = map_file.try_get();
        let default_key = self.entities_container.default_key.clone();
        if let Some(map) = map {
            map.render.render_background(&mut RenderPipeline::new(
                &map.data.buffered_map.map_visual,
                &map.data.buffered_map,
                &Default::default(),
                &cur_time,
                &cur_time,
                &Camera {
                    pos: vec2::new(x, y),
                    zoom,
                },
                &mut self.entities_container,
                Some(&default_key),
                "ddnet",
                1.0,
            ));

            let mut state = State::new();
            Self::map_canvas_for_players(&self.graphics, &mut state, 0.0, 0.0, zoom);
            let mut anim_state = AnimState::default();
            anim_state.set(&base_anim(), &Duration::from_millis(0));
            if params.in_air.unwrap_or_default() {
                anim_state.add(&inair_anim(), &Duration::from_millis(0), 1.0);
            } else {
                anim_state.add(&idle_anim(), &Duration::from_millis(0), 1.0);
            }
            let skin_name: Option<NetworkResourceKey<24>> = skin_name.as_str().try_into().ok();
            let skin = self.skin_container.get_or_default_opt(skin_name.as_ref());

            let mut render_info = CharacterRenderInfo {
                lerped_pos: Default::default(),
                lerped_vel: Default::default(),
                lerped_hook_pos: Default::default(),
                has_air_jump: Default::default(),
                cursor_pos: dvec2::new(dir.x as f64, dir.y as f64),
                move_dir: Default::default(),
                cur_weapon: Default::default(),
                recoil_ticks_passed: Default::default(),
                left_eye: Default::default(),
                right_eye: Default::default(),
                buffs: PoolLinkedHashMap::new_without_pool(),
                debuffs: PoolLinkedHashMap::new_without_pool(),
                animation_ticks_passed: Default::default(),
                game_ticks_passed: Default::default(),
                game_round_ticks: Default::default(),
                emoticon: Default::default(),
            };
            // tee info
            let color_body = if !custom_color {
                TeeRenderSkinColor::Original
            } else {
                let _a = ((color_body >> 24) & 0xFF) as f64 / 255.0;
                let h = ((color_body >> 16) & 0xFF) as f64 / 255.0;
                let s = ((color_body >> 8) & 0xFF) as f64 / 255.0;
                let l = ((color_body) & 0xFF) as f64 / 255.0;
                let mut hsl = palette::Hsl::new_const((h * 360.0).into(), s, l);
                let darkest = 0.5;
                hsl.lightness = darkest + hsl.lightness * (1.0 - darkest);

                let rgb = palette::rgb::LinSrgb::from_color_unclamped(hsl);
                TeeRenderSkinColor::Colorable(ColorRgba {
                    r: rgb.red as f32,
                    g: rgb.green as f32,
                    b: rgb.blue as f32,
                    a: 1.0,
                })
            };

            let color_feet = if !custom_color {
                TeeRenderSkinColor::Original
            } else {
                let _a = ((color_feet >> 24) & 0xFF) as f64 / 255.0;
                let h = ((color_feet >> 16) & 0xFF) as f64 / 255.0;
                let s = ((color_feet >> 8) & 0xFF) as f64 / 255.0;
                let l = ((color_feet) & 0xFF) as f64 / 255.0;
                let mut hsl = palette::Hsl::new_const((h * 360.0).into(), s, l);
                let darkest = 0.5;
                hsl.lightness = darkest + hsl.lightness * (1.0 - darkest);

                let rgb = palette::rgb::LinSrgb::from_color_unclamped(hsl);
                TeeRenderSkinColor::Colorable(ColorRgba {
                    r: rgb.red as f32,
                    g: rgb.green as f32,
                    b: rgb.blue as f32,
                    a: 1.0,
                })
            };

            let tee_render_info = TeeRenderInfo {
                eye_left: tee_eyes,
                eye_right: tee_eyes,
                color_body,
                color_feet,
                got_air_jump: !params.used_air_jump.unwrap_or_default(),
                feet_flipped: false,
                size: 2.0,
            };

            // hook
            let hook_hand = hook_pos.and_then(|hook_pos| {
                render_info.lerped_hook_pos = Some(hook_pos);
                self.toolkit_renderer.render_hook_for_player(
                    &mut self.hooks_container,
                    None,
                    vec2::default(),
                    &render_info,
                    state,
                )
            });
            if let Some(hook_hand) = hook_hand {
                self.tee_renderer.render_tee_hand(
                    &RenderTeeHandMath::new(&vec2::default(), 2.0, &hook_hand),
                    &color_body,
                    skin,
                    1.0,
                    &state,
                );
            }

            let weapon_hand = if let Some(weapon_ty) = weapon {
                render_info.cur_weapon = weapon_ty;

                let weapon = self.weapon_container.default_key.clone();
                let weapons = self.weapon_container.get_or_default(&weapon);
                self.toolkit_renderer.render_weapon_for_player(
                    weapons,
                    &render_info,
                    Default::default(),
                    50.try_into().unwrap(),
                    &GameTimeInfo {
                        ticks_per_second: 50.try_into().unwrap(),
                        intra_tick_time: Duration::ZERO,
                    },
                    state,
                    false,
                    false,
                )
            } else {
                None
            };

            self.tee_renderer.render_tee(
                &anim_state,
                skin,
                &tee_render_info,
                &TeeRenderHands {
                    left: None,
                    right: weapon_hand,
                },
                &dir,
                &vec2::new(0.0, 0.0),
                1.0,
                &state,
            );

            if let Some(emoticon) = emoticon {
                let emoticon_key = self.emoticon_container.default_key.clone();
                self.emoticon_renderer.render(&mut RenderEmoticonPipe {
                    emoticon_container: &mut self.emoticon_container,
                    pos: vec2::new(0.0, 0.0),
                    state: &state,
                    emoticon_key: Some(&emoticon_key),
                    emoticon,
                    emoticon_ticks: 90,
                    intra_tick_time: Duration::ZERO,
                    ticks_per_second: 50.try_into().unwrap(),
                });
            }

            let name = if let Some(name) = &params.player_name {
                Some(name)
            } else {
                None
            };

            if let Some(name) = name {
                self.nameplate_renderer.render(&mut NameplateRenderPipe {
                    cur_time: &self.sys.time_get_nanoseconds(),
                    name,
                    state: &state,
                    pos: &vec2::new(0.0, 0.0),
                    camera_zoom: zoom.clamp(0.3, f32::MAX),
                });
            }

            map.render.render_foreground(&mut RenderPipeline::new(
                &map.data.buffered_map.map_visual,
                &map.data.buffered_map,
                &Default::default(),
                &cur_time,
                &cur_time,
                &Camera {
                    pos: vec2::new(x, y),
                    zoom,
                },
                &mut self.entities_container,
                Some(&default_key),
                "ddnet",
                1.0,
            ));
        }

        #[derive(Debug)]
        struct Screenshot {
            sender: RefCell<Option<Sender<anyhow::Result<Vec<u8>>>>>,
        }
        impl ScreenshotCb for Screenshot {
            fn on_screenshot(&self, png: anyhow::Result<Vec<u8>>) {
                if let Some(sender) = self.sender.borrow_mut().take() {
                    let _ = sender.send(png);
                }
            }
        }
        let cb = Screenshot {
            sender: RefCell::new(Some(sender)),
        };
        self.graphics.do_screenshot(cb).unwrap();
        self.graphics.swap();
        self.graphics_backend.wait_idle().unwrap();
        self.graphics.check_pending_screenshot();

        self.skin_container.update(
            &self.sys.time_get_nanoseconds(),
            &Duration::from_secs(5),
            &Duration::from_secs(1),
            [].into_iter(),
        );
    }

    pub fn wait_skin_loaded(&mut self, skin_name: &str) {
        let Ok(skin_key): Result<ResourceKey, _> = skin_name.try_into() else {
            return;
        };
        self.skin_container.blocking_wait_loaded(&skin_key);
    }

    fn new(loading: ClientLoad) -> anyhow::Result<Self> {
        // then prepare components allocations etc.
        let tp = loading.tp.clone();

        let width: u32 = std::env::var("WIDTH")
            .map_err(|err| anyhow!(err))
            .and_then(|s| s.parse::<u32>().map_err(|err| anyhow!(err)))
            .unwrap_or(800);
        let height: u32 = std::env::var("HEIGHT")
            .map_err(|err| anyhow!(err))
            .and_then(|s| s.parse::<u32>().map_err(|err| anyhow!(err)))
            .unwrap_or(600);

        let (backend_base, streamed_data) = GraphicsBackendBase::new(
            loading.backend_loading_io,
            loading.backend_loading,
            &tp,
            BackendWindow::Headless { width, height },
        )?;

        let window_props = backend_base.get_window_props();
        let graphics_backend = GraphicsBackend::new(backend_base);
        let graphics = Graphics::new(graphics_backend.clone(), streamed_data, window_props);

        let tee_renderer = RenderTee::new(&graphics);
        let mut creator = UiCreator::default();
        let font_loading = UiFontDataLoading::new(&loading.io);
        let font_data = UiFontData::new(font_loading)?;
        creator.load_font(&font_data);
        let nameplate_renderer = NameplateRender::new(&graphics, &creator);
        let emoticon_renderer = RenderEmoticon::new(&graphics);
        let toolkit_renderer = ToolkitRender::new(&graphics);

        let sound_backend = SoundBackend::new(&ConfigSound {
            backend: "None".to_string(),
        })?;
        let sound = SoundManager::new(sound_backend.clone())?;
        let scene = sound.scene_handle.create(Default::default());

        let default_skin = SkinContainer::load_default(&loading.io, SKIN_CONTAINER_PATH.as_ref());
        let skins = SkinContainer::new(
            loading.io.clone(),
            tp.clone(),
            default_skin,
            None,
            None,
            "skin-container",
            &graphics,
            &sound,
            &scene,
            SKIN_CONTAINER_PATH.as_ref(),
        );
        let default_entities =
            EntitiesContainer::load_default(&loading.io, ENTITIES_CONTAINER_PATH.as_ref());
        let entities = EntitiesContainer::new(
            loading.io.clone(),
            tp.clone(),
            default_entities,
            None,
            None,
            "entities-container",
            &graphics,
            &sound,
            &scene,
            ENTITIES_CONTAINER_PATH.as_ref(),
        );
        let default_emoticons =
            EmoticonsContainer::load_default(&loading.io, EMOTICONS_CONTAINER_PATH.as_ref());
        let emoticons_container = EmoticonsContainer::new(
            loading.io.clone(),
            tp.clone(),
            default_emoticons,
            None,
            None,
            "emoticon-container",
            &graphics,
            &sound,
            &scene,
            EMOTICONS_CONTAINER_PATH.as_ref(),
        );
        let default_weapons =
            WeaponContainer::load_default(&loading.io, WEAPON_CONTAINER_PATH.as_ref());
        let weapons_container = WeaponContainer::new(
            loading.io.clone(),
            tp.clone(),
            default_weapons,
            None,
            None,
            "weapons-container",
            &graphics,
            &sound,
            &scene,
            WEAPON_CONTAINER_PATH.as_ref(),
        );
        let default_hook = HookContainer::load_default(&loading.io, HOOK_CONTAINER_PATH.as_ref());
        let hooks_container = HookContainer::new(
            loading.io.clone(),
            tp.clone(),
            default_hook,
            None,
            None,
            "hooks-container",
            &graphics,
            &sound,
            &scene,
            HOOK_CONTAINER_PATH.as_ref(),
        );

        let fs = loading.io.fs.clone();
        let ctf1 = loading
            .io
            .io_batcher
            .spawn(async move { Ok(fs.read_file("map/maps/ctf1.twmap".as_ref()).await?) })
            .get_storage()
            .unwrap();

        let mut client_map = ClientMapRender::new(RenderMapLoading::new(
            tp.clone(),
            ctf1,
            None,
            loading.io.clone(),
            &sound,
            Default::default(),
            &graphics,
            &Default::default(),
        ));

        while client_map.continue_loading(&Default::default()).is_none() {
            std::thread::sleep(Duration::from_millis(10));
            std::thread::yield_now();
        }

        println!("finished setup");

        graphics.swap();

        Ok(Self {
            graphics_backend,
            graphics,

            tee_renderer,
            emoticon_renderer,
            nameplate_renderer,
            toolkit_renderer,

            skin_container: skins,
            entities_container: entities,
            emoticon_container: emoticons_container,
            weapon_container: weapons_container,
            hooks_container,

            client_map,
            sys: loading.sys,
        })
    }

    fn run(self) {
        *CLIENT.blocking_lock() = Some(ClientWrapper(self));

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2) // should be at least 2
            .max_blocking_threads(2) // must be at least 2
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();

        let init_discord = std::env::var("DISCORD_TOKEN").is_ok();

        if init_discord {
            rt.block_on(async move { tokio::join!(async_main(), async_main_discord()) });
        } else {
            rt.block_on(async_main());
        }
    }
}

fn main() {
    if std::env::var("RUST_LOG").is_err() {
        unsafe { std::env::set_var("RUST_LOG", "warn,df::tract=error") };
    }
    env_logger::init();

    dotenvy::dotenv().ok();

    let io = Io::new(
        |runtime| {
            Arc::new(FileSystem::new(
                runtime,
                "org",
                "",
                "DDNet_Webservice",
                "DDNet_Accounts_Dummy",
            ))
        },
        Arc::new(HttpClient::new()),
    );
    let tp = Arc::new(ThreadPoolBuilder::new().build().unwrap());

    let map_pipe = MapPipeline::new_boxed();

    let config_gl = config_gl();
    let config_gfx = config_gfx();
    let loading = GraphicsBackendLoading::new(
        &config_gfx,
        &config_dbg(),
        &config_gl,
        graphics_backend::window::BackendRawDisplayHandle::Headless,
        Some(Arc::new(parking_lot::RwLock::new(vec![map_pipe]))),
        io.clone().into(),
    )
    .unwrap();
    let loading_io = GraphicsBackendIoLoading::new(&config_gfx, &io.clone().into());

    let sys = System::new();

    let client = Client::new(ClientLoad {
        backend_loading: loading,
        backend_loading_io: loading_io,
        sys,
        io,
        tp,
    })
    .unwrap();
    client.run();
}

async fn async_main() {
    let app = Router::new().route("/", get(generate_preview));

    let port: u16 = std::env::var("PORT")
        .map_err(|err| anyhow!(err))
        .and_then(|s| s.parse::<u16>().map_err(|err| anyhow!(err)))
        .unwrap_or(3002);

    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Command(command) = interaction {
            let guild_id = GuildId::new(
                std::env::var("GUILD_ID")
                    .expect("Expected GUILD_ID in environment")
                    .parse()
                    .expect("GUILD_ID must be an integer"),
            );

            let on_err = |err: String| {
                Box::pin(async {
                    let _ = command
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .ephemeral(true)
                                    .content(err),
                            ),
                        )
                        .await;
                })
            };

            if command.guild_id != Some(guild_id) {
                return on_err("can only be used in the right discord".into()).await;
            }

            let main_cmd_str = Mention::User(command.user.id).to_string()
                + "\n\
                You preview has finished\n\n\
                ";
            let content = match command.data.name.as_str() {
                "skin" => Some(main_cmd_str.clone()),
                _ => None,
            };

            let player_name = if let Some(arg) = command
                .data
                .options
                .first()
                .and_then(|arg| arg.value.as_str())
            {
                arg.to_string()
            } else {
                "".to_string()
            };

            if let Some(content) = content {
                let img =
                    match HTTP
                        .get(
                            format!(
                                "http://localhost:3002/?player_name={}\
                                &skin_name=default\
                                &zoom=0.25\
                                &x=17.0\
                                &y=25.5\
                                &weapon=hammer\
                                &emoticon=hearts\
                                &use_player_api=true\
                                &eyes=happy",
                                encode(&player_name)
                            )
                            .as_str(),
                        )
                        .send()
                        .await
                        .and_then(|res| res.error_for_status())
                    {
                        Ok(skin) => {
                            if skin.headers().get("content-type").is_some_and(|ty| {
                                ty.to_str().is_ok_and(|ty| ty.contains("image/png"))
                            }) {
                                match skin.bytes().await {
                                    Ok(skin) => skin,
                                    Err(err) => return on_err(err.to_string()).await,
                                }
                            } else {
                                return on_err(format!(
                                    "Failed to fetch image: {}",
                                    skin.text().await.unwrap_or_else(|err| err.to_string())
                                ))
                                .await;
                            }
                        }
                        Err(err) => {
                            return on_err(err.to_string()).await;
                        }
                    };

                let data = CreateInteractionResponseMessage::new()
                    .content(content)
                    //.ephemeral(true)
                    .add_file(CreateAttachment::bytes(img, "preview.png"));
                let builder = CreateInteractionResponse::Message(data);
                if let Err(why) = command.create_response(&ctx.http, builder).await {
                    println!("Could not respond to slash command: {why}");
                } else {
                    let _ = ctx.data.write().await;
                }
            }
        }
    }

    async fn ready(&self, ctx: Context, _ready: Ready) {
        let guild_id = GuildId::new(
            std::env::var("GUILD_ID")
                .expect("Expected GUILD_ID in environment")
                .parse()
                .expect("GUILD_ID must be an integer"),
        );

        let skin_cmd = CreateCommand::new("skin")
            .description("Create a preview of that skin")
            .add_option(CreateCommandOption::new(
                serenity::all::CommandOptionType::String,
                "player_name",
                "Name of the player to render",
            ))
            .dm_permission(false);

        if (guild_id.set_commands(&ctx.http, vec![skin_cmd]).await).is_err() {
            // ignore for now
        }
    }
}

async fn async_main_discord() {
    let framework = StandardFramework::new();

    // Login with a bot token from the environment
    let token = std::env::var("DISCORD_TOKEN").expect("token");
    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    let mut client = serenity::Client::builder(token, intents)
        .event_handler(Handler)
        .framework(framework)
        .await
        .expect("Error creating client");

    // start listening for events by starting a single shard
    if let Err(why) = client.start().await {
        panic!("An error occurred while running the client: {why:?}");
    }
}

async fn generate_preview(params: Option<Query<RenderParams>>) -> impl IntoResponse {
    if let Some(Query(mut params)) = params {
        if params.use_player_api.is_some_and(|b| b) {
            let can_update = {
                let mut g = PLAYERS.lock();
                let now = &mut *g;
                let can_update =
                    std::time::Instant::now().duration_since(*now) > Duration::from_millis(500);
                if can_update {
                    *now = std::time::Instant::now();
                } else {
                    return "Rate limited".into_response();
                }
                can_update
            };

            if can_update && params.player_name.is_some() {
                if let Ok(skin) = HTTP
                    .get(
                        format!(
                            "https://ddstats.tw/profile/json?player={}",
                            encode(params.player_name.as_ref().unwrap())
                        )
                        .as_str(),
                    )
                    .send()
                    .await
                {
                    if let Ok(skin) =
                        skin.text().await.map_err(|err| anyhow!(err)).and_then(|s| {
                            serde_json::from_str::<Skin>(&s).map_err(|err| anyhow!(err))
                        })
                    {
                        params.player_name = params.player_name.clone();
                        params.skin_name = skin.name;
                        params.body = skin.color_body;
                        params.feet = skin.color_feet;
                    }
                }
            };
        }

        let (sender, receiver) = oneshot::channel();
        tokio::task::spawn_blocking(|| {
            let mut client = CLIENT.blocking_lock();
            let client = client.as_mut().unwrap();
            client.0.wait_skin_loaded(&params.skin_name);
            client.0.render(params, sender)
        })
        .await
        .unwrap();

        let img = receiver.await.unwrap().unwrap();

        let cursor = Cursor::new(img);
        let stream = ReaderStream::new(cursor);
        // convert the `Stream` into an `axum::body::HttpBody`
        let body = StreamBody::new(stream);
        let headers = [(header::CONTENT_TYPE, "image/png; charset=utf-8")];
        (headers, body).into_response()
    } else {
        format!(
            "Non optional render parameters missing: {:?}",
            RenderParams::default()
        )
        .into_response()
    }
}
