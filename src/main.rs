use axum::{
    body::StreamBody,
    extract::Query,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base::system::System;
use base_fs::{filesys::FileSystem, io_batcher::TokIOBatcher};
use client_render::{
    containers::{
        entities::EntitiesContainer,
        skins::{SkinContainer, TeeSkinEye},
    },
    map::{
        client_map::{ClientMap, ClientMapFile},
        render_pipe::{Camera, RenderPipeline, RenderPipelineBase},
        render_tools::RenderTools,
    },
    render::{
        animation::AnimState,
        default_anim::{base_anim, idle_anim},
        tee::{RenderTee, TeeRenderHands, TeeRenderInfo, TeeRenderSkinTextures},
    },
};
use config::config::{Config, ConfigGFX, ConfigMap};
use graphics_backend::{
    types::{Graphics, GraphicsBackendLoadIOPipe, GraphicsBackendLoadWhileIOPipe},
    window::BackendWindow,
};
use graphics_base_traits::traits::GraphicsSizeQuery;
use graphics_types::rendering::{ColorRGBA, State};
use math::math::{normalize, vector::vec2};
use palette::{
    convert::{FromColorUnclamped, IntoColorUnclamped},
    IntoColor,
};
use serde::{Deserialize, Serialize};
use std::{io::Cursor, net::SocketAddr, num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;

use shared_game::state::state::GameStateInterface;

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
}

struct ClientLoad {
    backend_base: graphics_backend::backend::GraphicsBackendBase,
    sys: System,
    fs: Arc<FileSystem>,
    io_batcher: TokIOBatcher,
}

struct Client {
    _backend: graphics_backend::backend::GraphicsBackend,
    graphics: Graphics,
    tee_renderer: RenderTee,
    skin_container: SkinContainer,
    entities_container: EntitiesContainer,
    fs: Arc<FileSystem>,
    io_batcher: TokIOBatcher,
    thread_pool: Arc<rayon::ThreadPool>,
    sys: System,
    client_map: ClientMap,
    client_map_pkm: ClientMap,
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
            100.0,
            center_x,
            center_y,
            graphics.canvas_aspect(),
            zoom,
        );
        state.map_canvas(points[0], points[1], points[2], points[3]);
    }

    pub fn render(&mut self, params: RenderParams) -> Vec<u8> {
        let mut skin_name = params.skin_name;

        let map_name = params.map_name.unwrap_or("ctf1".to_string());
        let is_ctf1 = map_name == "ctf1";

        let default_x = if is_ctf1 { 173.12 } else { 1358.08 };
        let default_y = if is_ctf1 { 688.96 } else { 24240.96 };

        if self
            .skin_names
            .iter()
            .find(|str| (*str).eq(&skin_name))
            .is_none()
        {
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

        let map = if is_ctf1 {
            &mut self.client_map
        } else {
            &mut self.client_map_pkm
        }
        .continue_loading(
            &self.io_batcher,
            &self.fs,
            &mut self.graphics,
            &Config::new(),
            &self.sys,
        );
        if let Some(_) = map {
            let (map, game) = if is_ctf1 {
                &mut self.client_map
            } else {
                &mut self.client_map_pkm
            }
            .unwrap_data_and_game_mut();

            if !self.did_tick {
                game.tick();
                self.did_tick = true;
            }

            map.render.render_background(&mut RenderPipeline {
                base: RenderPipelineBase {
                    map: &map.raw,
                    map_images: &map.images,
                    config: &ConfigMap::default(),
                    graphics: &mut self.graphics,
                    sys: &self.sys,
                    intra_tick_time: &Duration::ZERO,
                    game: game,
                    camera: &Camera {
                        pos: vec2::new(x, y),
                        zoom: zoom,
                        animation_start_tick: 0,
                    },
                    entities_container: &mut self.entities_container,
                    fs: &self.fs,
                    io_batcher: &self.io_batcher,
                    runtime_thread_pool: &self.thread_pool,
                    force_full_design_render: true,
                },
                buffered_map: &map.buffered_map,
            });

            let mut state = State::new();
            Self::map_canvas_for_players(&self.graphics, &mut state, 0.0, 0.0, zoom);
            let mut anim_state = AnimState::default();
            anim_state.set(&base_anim(), &Duration::from_millis(0));
            anim_state.add(&idle_anim(), &Duration::from_millis(0), 1.0);
            let skin = self.skin_container.get_or_default(
                &skin_name,
                &mut self.graphics,
                &self.fs,
                &self.io_batcher,
                &self.thread_pool,
            );

            let color_body = if !custom_color {
                ColorRGBA {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                }
            } else {
                let _a = ((color_body >> 24) & 0xFF) as f64 / 255.0;
                let h = ((color_body >> 16) & 0xFF) as f64 / 255.0;
                let s = ((color_body >> 8) & 0xFF) as f64 / 255.0;
                let l = ((color_body >> 0) & 0xFF) as f64 / 255.0;
                let mut hsl = palette::Hsl::new_const((h * 360.0).into(), s, l);
                let darkest = 0.5;
                hsl.lightness = darkest + hsl.lightness * (1.0 - darkest);

                let rgb = palette::rgb::LinSrgb::from_color_unclamped(hsl);
                ColorRGBA {
                    r: rgb.red as f32,
                    g: rgb.green as f32,
                    b: rgb.blue as f32,
                    a: 1.0,
                }
            };

            let color_feet = if !custom_color {
                ColorRGBA {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                }
            } else {
                let _a = ((color_feet >> 24) & 0xFF) as f64 / 255.0;
                let h = ((color_feet >> 16) & 0xFF) as f64 / 255.0;
                let s = ((color_feet >> 8) & 0xFF) as f64 / 255.0;
                let l = ((color_feet >> 0) & 0xFF) as f64 / 255.0;
                let mut hsl = palette::Hsl::new_const((h * 360.0).into(), s, l);
                let darkest = 0.5;
                hsl.lightness = darkest + hsl.lightness * (1.0 - darkest);

                let rgb = palette::rgb::LinSrgb::from_color_unclamped(hsl);
                ColorRGBA {
                    r: rgb.red as f32,
                    g: rgb.green as f32,
                    b: rgb.blue as f32,
                    a: 1.0,
                }
            };

            let tee_render_info = TeeRenderInfo {
                render_skin: if custom_color {
                    TeeRenderSkinTextures::Colorable(&skin.grey_scaled_textures)
                } else {
                    TeeRenderSkinTextures::Original(&skin.textures)
                },
                color_body,
                color_feet,
                metrics: &skin.metrics,
                got_air_jump: true,
                feet_flipped: false,
                size: 64.0,
            };

            self.tee_renderer.render_tee(
                &mut self.graphics,
                &anim_state,
                &tee_render_info,
                TeeSkinEye::Normal,
                &TeeRenderHands {
                    left: None,
                    right: None,
                },
                &dir,
                &vec2::new(0.0, 0.0),
                1.0,
                &state,
            );

            map.render.render_foreground(&mut RenderPipeline {
                base: RenderPipelineBase {
                    map: &map.raw,
                    map_images: &map.images,
                    config: &ConfigMap::default(),
                    graphics: &mut self.graphics,
                    sys: &self.sys,
                    intra_tick_time: &Duration::ZERO,
                    game: game,
                    camera: &Camera {
                        pos: vec2::new(x, y),
                        zoom: zoom,
                        animation_start_tick: 0,
                    },
                    entities_container: &mut self.entities_container,
                    fs: &self.fs,
                    io_batcher: &self.io_batcher,
                    runtime_thread_pool: &self.thread_pool,
                    force_full_design_render: true,
                },
                buffered_map: &map.buffered_map,
            });
        }

        self.graphics.swap();
        let png = self.graphics.do_screenshot().unwrap();

        png
    }

    fn new(mut loading: ClientLoad) -> Self {
        // then prepare components allocations etc.
        let thread_pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(
                    std::thread::available_parallelism()
                        .unwrap_or(NonZeroUsize::new(2).unwrap())
                        .get()
                        .max(4)
                        - 2,
                )
                .build()
                .unwrap(),
        );

        let mut pipe = GraphicsBackendLoadWhileIOPipe {
            runtime_threadpool: &thread_pool,
            config: &Config::default(),
            sys: &loading.sys,
            window_handling: BackendWindow::Headless {
                width: 800,
                height: 600,
            },
        };
        loading.backend_base.init_while_io(&mut pipe);
        let stream_data = loading.backend_base.init().unwrap();
        let window_props = *loading.backend_base.get_window_props();

        let backend = graphics_backend::backend::GraphicsBackend::new(loading.backend_base);

        let mut graphics = Graphics::new(backend.clone(), stream_data, window_props);

        let tee_renderer = RenderTee::new(&mut graphics);

        let default_skin = SkinContainer::load(
            "default",
            &loading.fs,
            &loading.io_batcher,
            thread_pool.clone(),
        );
        let mut skins = SkinContainer::new(default_skin);
        let default_entities = EntitiesContainer::load(
            "default",
            &loading.fs,
            &loading.io_batcher,
            thread_pool.clone(),
        );
        let entities = EntitiesContainer::new(default_entities);

        let fs = loading.fs.clone();
        let skin_names = loading
            .io_batcher
            .spawn(async move {
                let mut files: Vec<String> = Default::default();
                fs.files_or_dirs_of_dir("skins", &mut |file| {
                    files.push(file);
                })
                .await;

                Ok(files)
            })
            .get_storage()
            .unwrap();

        skin_names.iter().for_each(|skin| {
            skins.get_or_default(
                skin,
                &mut graphics,
                &loading.fs,
                &loading.io_batcher,
                &thread_pool,
            );
        });

        let client_map = ClientMap::UploadingImagesAndMapBuffer(ClientMapFile::new(
            &thread_pool,
            "ctf1",
            &loading.io_batcher,
            &mut graphics,
            &loading.fs,
            &Config::default(),
            &loading.sys.time,
        ));

        let client_map_pkm = ClientMap::UploadingImagesAndMapBuffer(ClientMapFile::new(
            &thread_pool,
            "pkm",
            &loading.io_batcher,
            &mut graphics,
            &loading.fs,
            &Config::default(),
            &loading.sys.time,
        ));

        println!("finished setup");

        Self {
            _backend: backend,
            graphics,
            tee_renderer,
            skin_container: skins,
            entities_container: entities,
            fs: loading.fs,
            io_batcher: loading.io_batcher,
            thread_pool,
            client_map,
            client_map_pkm,
            sys: loading.sys,
            skin_names,
            did_tick: false,
        }
    }

    fn run(&mut self) {
        *unsafe { &mut CLIENT } = Some(Mutex::new(self));

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2) // should be at least 2
            .max_blocking_threads(2) // must be at least 2
            .enable_all()
            .build()
            .unwrap();
        let _g = rt.enter();
        rt.block_on(asnyc_main());
    }
}

fn main() {
    let mut backend_base = graphics_backend::backend::GraphicsBackendBase::new();

    let sys = System::new();
    let fs = Arc::new(FileSystem::new(&sys.log, "org", "", "DDNet_Webservice"));

    // tokio runtime for client side tasks
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4) // should be at least 2
        .max_blocking_threads(4) // must be at least 2
        .build()
        .unwrap();

    let io_batcher = TokIOBatcher::new(rt);

    let mut io_pipe = GraphicsBackendLoadIOPipe {
        fs: &fs,
        io_batcher: &io_batcher,
        config: &ConfigGFX {
            ..Default::default()
        },
    };
    backend_base.load_io(&mut io_pipe);

    let mut client = Client::new(ClientLoad {
        backend_base,
        sys,
        fs,
        io_batcher,
    });
    client.run();
}

async fn asnyc_main() {
    // build our application with a route
    let app = Router::new()
        // `GET /` goes to `root`
        .route("/", get(root));

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from(([127, 0, 0, 1], 3002));

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

fn render_global(params: RenderParams) -> Vec<u8> {
    unsafe { (**CLIENT.as_mut().unwrap().blocking_lock()).render(params) }
}

// basic handler that responds with a static string
async fn root(params: Option<Query<RenderParams>>) -> impl IntoResponse {
    if let Some(params) = params {
        let params = params.0;
        let img = tokio::task::spawn_blocking(|| render_global(params))
            .await
            .unwrap();

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
