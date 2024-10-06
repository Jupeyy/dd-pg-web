use axum::{
    async_trait, body::StreamBody, extract::Query, http::header, response::IntoResponse,
    routing::get, Router,
};
use base::system::System;
use base_fs::filesys::FileSystem;
use base_http::http::HttpClient;
use base_io::io::Io;
use client_containers::{
    entities::{EntitiesContainer, ENTITIES_CONTAINER_PATH},
    skins::{SkinContainer, SKIN_CONTAINER_PATH},
};
use client_render_base::{
    map::{
        map_pipeline::MapPipeline,
        render_pipe::{Camera, RenderPipeline},
        render_tools::RenderTools,
    },
    render::{
        animation::AnimState,
        default_anim::{base_anim, idle_anim},
        tee::{RenderTee, TeeRenderHands, TeeRenderInfo, TeeRenderSkinColor},
    },
};
use client_render_game::map::render_map_base::{ClientMapRender, RenderMapLoading};
use config::config::{ConfigBackend, ConfigDebug, ConfigGfx, ConfigSound, GfxDebugModes};
use game_interface::types::{render::character::TeeEye, resource_key::NetworkResourceKey};
use graphics::graphics::graphics::{Graphics, ScreenshotCb};
use graphics_backend::{
    backend::{
        GraphicsBackend, GraphicsBackendBase, GraphicsBackendIoLoading, GraphicsBackendLoading,
    },
    window::BackendWindow,
};

use graphics_backend_traits::traits::GraphicsBackendInterface;

use graphics_types::rendering::{ColorRgba, State};
use math::math::{normalize, vector::vec2};
use palette::convert::FromColorUnclamped;
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde::Deserialize;
use serenity::all::{
    Context, CreateAttachment, CreateCommand, CreateInteractionResponse,
    CreateInteractionResponseMessage, EventHandler, GatewayIntents, GuildId, Interaction, Mention,
    Ready, StandardFramework,
};
use sound::sound::SoundManager;
use sound_backend::sound_backend::SoundBackend;
use std::{
    cell::RefCell, io::Cursor, net::SocketAddr, ptr::addr_of_mut, rc::Rc, sync::Arc, time::Duration,
};
use tokio::sync::{
    oneshot::{self, Sender},
    Mutex,
};
use tokio_util::io::ReaderStream;

static mut CLIENT: Option<Mutex<*mut Client>> = None;

#[derive(Debug, Default, Deserialize)]
struct RenderParams {
    skin_name: String,
    zoom: Option<f32>,
    x: Option<f32>,
    y: Option<f32>,
    map_name: Option<String>,
    body: Option<i32>,
    feet: Option<i32>,
    dir_x: Option<f32>,
    dir_y: Option<f32>,
    eyes: Option<String>,
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
    sound: SoundManager,
    tee_renderer: RenderTee,
    skin_container: SkinContainer,
    entities_container: EntitiesContainer,
    io: Io,
    thread_pool: Arc<rayon::ThreadPool>,
    sys: System,
    client_map: ClientMapRender,
    client_map_pkm: ClientMapRender,
    skin_names: Vec<String>,
    did_tick: bool,
}

impl Client {
    pub fn map_canvas_for_players(
        graphics: &Graphics,
        state: &mut State,
        center_x: f32,
        center_y: f32,
        zoom: f32,
    ) {
        let points: [f32; 4] = RenderTools::map_canvas_to_world(
            0.0,
            0.0,
            0.0,
            0.0,
            center_x,
            center_y,
            graphics.canvas_handle.canvas_aspect(),
            zoom,
        );
        state.map_canvas(points[0], points[1], points[2], points[3]);
    }

    pub fn render(&mut self, params: RenderParams, sender: Sender<anyhow::Result<Vec<u8>>>) {
        let mut skin_name = params.skin_name;

        let map_name = params.map_name.unwrap_or("ctf1".to_string());
        let is_ctf1 = map_name == "ctf1";

        let default_x = if is_ctf1 { 173.12 } else { 1358.08 };
        let default_y = if is_ctf1 { 688.96 } else { 24240.96 };

        if !self.skin_names.iter().any(|str| (*str).eq(&skin_name)) {
            skin_name = "default".to_string();
        }

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
            _ => TeeEye::Normal,
        };

        let map_file = if is_ctf1 {
            &mut self.client_map
        } else {
            &mut self.client_map_pkm
        };
        let map = map_file.continue_loading(&Default::default());
        let default_key = self.entities_container.default_key.clone();
        if let Some(map) = map {
            map.render.render_background(&mut RenderPipeline::new(
                &map.data.buffered_map.map_visual,
                &map.data.buffered_map,
                &Default::default(),
                &Duration::ZERO,
                &Duration::ZERO,
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
            anim_state.add(&idle_anim(), &Duration::from_millis(0), 1.0);
            let skin_name: Option<NetworkResourceKey<24>> = skin_name.as_str().try_into().ok();
            let skin = self.skin_container.get_or_default_opt(skin_name.as_ref());

            let color_body = if !custom_color {
                TeeRenderSkinColor::Original
            } else {
                let _a = ((color_body >> 24) & 0xFF) as f64 / 255.0;
                let h = ((color_body >> 16) & 0xFF) as f64 / 255.0;
                let s = ((color_body >> 8) & 0xFF) as f64 / 255.0;
                let l = ((color_body >> 0) & 0xFF) as f64 / 255.0;
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
                let l = ((color_feet >> 0) & 0xFF) as f64 / 255.0;
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
                eye_left: TeeEye::Happy,
                eye_right: TeeEye::Happy,
                color_body,
                color_feet,
                got_air_jump: true,
                feet_flipped: false,
                size: 2.0 / zoom,
            };

            self.tee_renderer.render_tee(
                &anim_state,
                skin,
                &tee_render_info,
                &TeeRenderHands {
                    left: None,
                    right: None,
                },
                &dir,
                &vec2::new(0.0, 0.0),
                1.0,
                &state,
            );

            map.render.render_foreground(&mut RenderPipeline::new(
                &map.data.buffered_map.map_visual,
                &map.data.buffered_map,
                &Default::default(),
                &Duration::ZERO,
                &Duration::ZERO,
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
    }

    fn new(loading: ClientLoad) -> anyhow::Result<Self> {
        // then prepare components allocations etc.
        let tp = loading.tp.clone();

        let config_gl = config_gl();
        let (backend_base, streamed_data) = GraphicsBackendBase::new(
            loading.backend_loading_io,
            loading.backend_loading,
            &tp,
            BackendWindow::Headless {
                width: 800,
                height: 600,
            },
            &config_dbg(),
            &config_gl,
        )?;

        let window_props = backend_base.get_window_props();
        let graphics_backend = GraphicsBackend::new(backend_base);
        let graphics = Graphics::new(graphics_backend.clone(), streamed_data, window_props);

        let tee_renderer = RenderTee::new(&graphics);

        let sound_backend = SoundBackend::new(&ConfigSound {
            backend: "None".to_string(),
        })?;
        let sound = SoundManager::new(sound_backend.clone())?;
        let scene = sound.scene_handle.create(Default::default());

        let default_skin = SkinContainer::load_default(&loading.io, SKIN_CONTAINER_PATH.as_ref());
        let mut skins = SkinContainer::new(
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

        let fs = loading.io.fs.clone();
        let skin_names = loading
            .io
            .io_batcher
            .spawn(async move {
                let files = fs.entries_in_dir("skins".as_ref()).await?;

                Ok(files)
            })
            .get_storage()
            .unwrap();

        skin_names.keys().for_each(|skin| {
            let key: Option<NetworkResourceKey<24>> = skin.as_str().try_into().ok();
            skins.get_or_default_opt(key.as_ref());
        });

        let fs = loading.io.fs.clone();
        let ctf1 = loading
            .io
            .io_batcher
            .spawn(async move { Ok(fs.read_file("map/maps/ctf1.twmap".as_ref()).await?) })
            .get_storage()
            .unwrap();

        let client_map = ClientMapRender::new(RenderMapLoading::new(
            tp.clone(),
            ctf1,
            None,
            loading.io.clone(),
            &sound,
            Default::default(),
            &graphics,
            &Default::default(),
        ));

        let fs = loading.io.fs.clone();
        let pkm = loading
            .io
            .io_batcher
            .spawn(async move { Ok(fs.read_file("map/maps/pkm.twmap".as_ref()).await?) })
            .get_storage()
            .unwrap();

        let client_map_pkm = ClientMapRender::new(RenderMapLoading::new(
            tp.clone(),
            pkm,
            None,
            loading.io.clone(),
            &sound,
            Default::default(),
            &graphics,
            &Default::default(),
        ));

        println!("finished setup");

        graphics.swap();

        Ok(Self {
            graphics_backend,
            graphics,
            tee_renderer,
            skin_container: skins,
            entities_container: entities,
            io: loading.io,
            thread_pool: tp,
            client_map,
            client_map_pkm,
            sys: loading.sys,
            skin_names: skin_names.into_keys().collect(),
            did_tick: false,
            sound,
        })
    }

    fn run(&mut self) {
        unsafe {
            *addr_of_mut!(CLIENT) = Some(Mutex::new(self));
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2) // should be at least 2
            .max_blocking_threads(2) // must be at least 2
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();
        rt.block_on(async_main_discord());
    }
}

fn main() {
    if std::env::var("RUST_LOG").is_err() {
        unsafe { std::env::set_var("RUST_LOG", "warn,df::tract=error") };
    }
    env_logger::init();

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
    let loading = GraphicsBackendLoading::new(
        &config_gfx(),
        &config_dbg(),
        &config_gl,
        graphics_backend::window::BackendRawDisplayHandle::Headless,
        Some(Arc::new(parking_lot::RwLock::new(vec![map_pipe]))),
        io.clone().into(),
    )
    .unwrap();
    let loading_io = GraphicsBackendIoLoading::new(&config_gfx(), &io.clone().into());

    let sys = System::new();

    let mut client = Client::new(ClientLoad {
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
    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root));

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from(([0, 0, 0, 0], 3002));

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

            let main_cmd_str = Mention::User(command.user.id).to_string()
                + "\n\
                You preview has finished\n\n\
                ";
            let content = match command.data.name.as_str() {
                "skin" => Some(main_cmd_str.clone()),
                _ => None,
            };

            if let Some(content) = content {
                let params = RenderParams {
                    skin_name: "greyfox".to_string(),
                    zoom: Some(0.5),
                    x: Some(17.0),
                    y: Some(25.5),
                    map_name: None,
                    body: None,
                    feet: None,
                    dir_x: None,
                    dir_y: None,
                    eyes: None,
                };
                let (sender, receiver) = oneshot::channel();
                tokio::task::spawn_blocking(|| render_global(params, sender))
                    .await
                    .unwrap();

                let img = receiver.await.unwrap().unwrap();

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
            .dm_permission(false);

        if (guild_id.set_commands(&ctx.http, vec![skin_cmd]).await).is_err() {
            // ignore for now
        }
    }
}

async fn async_main_discord() {
    let framework = StandardFramework::new();

    /*
    for ez debugging
    env::set_var("GUILD_ID", "");
    env::set_var("ROLE_ID", "");
    env::set_var(
        "DISCORD_TOKEN",
        "",
    );
    env::set_var("USERNAME", "");
    env::set_var("PASSWORD", "");
    */

    dotenvy::dotenv().ok();

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

fn render_global(params: RenderParams, sender: Sender<anyhow::Result<Vec<u8>>>) {
    unsafe { (**CLIENT.as_mut().unwrap().blocking_lock()).render(params, sender) }
}

// basic handler that responds with a static string
async fn root(params: Option<Query<RenderParams>>) -> impl IntoResponse {
    if let Some(params) = params {
        let params = params.0;
        let (sender, receiver) = oneshot::channel();
        tokio::task::spawn_blocking(|| render_global(params, sender))
            .await
            .unwrap();

        let img = receiver.await.unwrap().unwrap();

        let cursor = Cursor::new(img);
        let stream = ReaderStream::new(cursor);
        // convert the `Stream` into an `axum::body::HttpBody`
        let body = StreamBody::new(stream);
        let headers = [
            (header::CONTENT_TYPE, "image/png; charset=utf-8"),
            /*(
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"img.png\"",
            ),*/
        ];

        (headers, body).into_response()
    } else {
        format!(
            "Non optional render parameters missing: {:?}",
            RenderParams::default()
        )
        .into_response()
    }
}
