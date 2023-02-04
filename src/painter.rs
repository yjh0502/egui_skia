use std::ops::Deref;
use std::sync::Arc;

use egui::epaint::ahash::AHashMap;
use egui::epaint::Primitive;
use egui::{ClippedPrimitive, ImageData, Pos2, TextureFilter, TextureId, TexturesDelta};
use skia_safe::runtime_effect::ChildPtr;
use skia_safe::vertices::VertexMode;
use skia_safe::{
    scalar, BlendMode, Canvas, ClipOp, Color, ColorSpace, ConditionallySend, Data, Drawable, Image,
    ImageInfo, Paint, PictureRecorder, Point, Rect, RuntimeEffect, Sendable, Surface, Vertices,
};

struct PaintHandle {
    paint: Paint,
    image: Image,
}

pub struct Painter {
    paints: AHashMap<TextureId, PaintHandle>,
}

const SKSL_SHADER: &'static str = r#"
uniform shader image;
vec4 main(float2 coord) {
    return image.eval(coord);
}
"#;

impl Painter {
    pub fn new() -> Painter {
        Self {
            paints: AHashMap::new(),
        }
    }

    pub fn paint_and_update_textures(
        &mut self,
        canvas: &mut Canvas,
        dpi: f32,
        primitives: Vec<ClippedPrimitive>,
        textures_delta: TexturesDelta,
    ) {
        textures_delta.set.iter().for_each(|(id, image_delta)| {
            let delta_image = match &image_delta.image {
                ImageData::Color(color_image) => Image::from_raster_data(
                    &ImageInfo::new_n32_premul(
                        skia_safe::ISize::new(
                            color_image.width() as i32,
                            color_image.height() as i32,
                        ),
                        None,
                    ),
                    Data::new_copy(
                        color_image
                            .pixels
                            .iter()
                            .flat_map(|p| p.to_array())
                            .collect::<Vec<_>>()
                            .as_slice(),
                    ),
                    color_image.width() * 4,
                )
                .unwrap(),
                ImageData::Font(font) => {
                    let pixels = font.srgba_pixels(Some(1.0));
                    Image::from_raster_data(
                        &ImageInfo::new_n32_premul(
                            skia_safe::ISize::new(font.width() as i32, font.height() as i32),
                            None,
                        ),
                        Data::new_copy(
                            pixels
                                .flat_map(|p| p.to_array())
                                .collect::<Vec<_>>()
                                .as_slice(),
                        ),
                        font.width() * 4,
                    )
                    .unwrap()
                }
            };

            let image = match image_delta.pos {
                None => delta_image,
                Some(pos) => {
                    let old_image = self.paints.remove(&id).unwrap().image;

                    let mut surface = Surface::new_raster_n32_premul(skia_safe::ISize::new(
                        old_image.width() as i32,
                        old_image.height() as i32,
                    ))
                    .unwrap();

                    let canvas = surface.canvas();

                    canvas.draw_image(&old_image, Point::new(0.0, 0.0), None);

                    canvas.clip_rect(
                        Rect::new(
                            pos[0] as scalar,
                            pos[1] as scalar,
                            (pos[0] as i32 + delta_image.width()) as scalar,
                            (pos[1] as i32 + delta_image.height()) as scalar,
                        ),
                        ClipOp::default(),
                        false,
                    );

                    canvas.clear(Color::TRANSPARENT);
                    canvas.draw_image(&delta_image, Point::new(pos[0] as f32, pos[1] as f32), None);

                    surface.image_snapshot()
                }
            };

            let local_matrix =
                skia_safe::Matrix::scale((1.0 / image.width() as f32, 1.0 / image.height() as f32));

            let sampling_options = {
                let filter_mode = match image_delta.options.magnification {
                    TextureFilter::Nearest => skia_safe::FilterMode::Nearest,
                    TextureFilter::Linear => skia_safe::FilterMode::Linear,
                };
                let mm_mode = if cfg!(feature = "cpu_fix") {
                    skia_safe::MipmapMode::None
                } else {
                    match image_delta.options.minification {
                        TextureFilter::Nearest => skia_safe::MipmapMode::Nearest,
                        TextureFilter::Linear => skia_safe::MipmapMode::Linear,
                    }
                };
                let sampling_options = skia_safe::SamplingOptions::new(filter_mode, mm_mode);
                sampling_options
            };
            let tile_mode = skia_safe::TileMode::Clamp;

            let mut paint = Paint::default();

            let mut shader = image
                .to_shader((tile_mode, tile_mode), sampling_options, &local_matrix)
                .unwrap();

            shader = RuntimeEffect::make_for_shader(SKSL_SHADER, None)
                .unwrap()
                .make_shader(Data::new_empty(), &[ChildPtr::Shader(shader)], None)
                .unwrap();

            paint.set_shader(shader);

            self.paints.insert(id.clone(), PaintHandle { paint, image });
        });

        for primitive in primitives {
            let skclip_rect = Rect::new(
                primitive.clip_rect.min.x,
                primitive.clip_rect.min.y,
                primitive.clip_rect.max.x,
                primitive.clip_rect.max.y,
            );
            match primitive.primitive {
                Primitive::Mesh(mesh) => {
                    canvas.set_matrix(&skia_safe::M44::new_identity().set_scale(dpi, dpi, 1.0));
                    let mut arc = skia_safe::AutoCanvasRestore::guard(canvas, true);

                    let meshes = mesh.split_to_u16();

                    for mesh in &meshes {
                        let texture_id = mesh.texture_id;

                        let mut pos = Vec::with_capacity(mesh.vertices.len());
                        let mut texs = Vec::with_capacity(mesh.vertices.len());
                        let mut colors = Vec::with_capacity(mesh.vertices.len());

                        let mut push_vert = |v: &egui::epaint::Vertex| {
                            // Apparently vertices can be NaN and if they are NaN, nothing is rendered.
                            // Replacing them with 0 works around this.
                            // https://github.com/lucasmerlin/egui_skia/issues/4
                            let fixed_pos = if v.pos.x.is_nan() || v.pos.y.is_nan() {
                                Pos2::new(0.0, 0.0)
                            } else {
                                v.pos
                            };

                            pos.push(Point::new(fixed_pos.x, fixed_pos.y));
                            texs.push(Point::new(v.uv.x, v.uv.y));

                            let c = v.color;
                            let c = Color::from_argb(c.a(), c.r(), c.g(), c.b());
                            // un-premultply color
                            let mut cf = skia_safe::Color4f::from(c);
                            cf.r /= cf.a;
                            cf.g /= cf.a;
                            cf.b /= cf.a;
                            colors.push(Color::from_argb(
                                c.a(),
                                (cf.r * 255.0) as u8,
                                (cf.g * 255.0) as u8,
                                (cf.b * 255.0) as u8,
                            ));
                        };

                        let mut i = 0;
                        while i < mesh.indices.len() {
                            let v0 = mesh.vertices[mesh.indices[i] as usize];
                            let mut v1 = mesh.vertices[mesh.indices[i + 1] as usize];
                            let mut v2 = mesh.vertices[mesh.indices[i + 2] as usize];
                            i += 3;

                            // Egui use the uv coordinates 0,0 to get a white color when drawing vector graphics
                            // 0,0 is always a white dot on the font texture
                            // Unfortunately skia has a bug where it cannot get a color when the uv coordinates are equal
                            // https://bugs.chromium.org/p/skia/issues/detail?id=13706
                            // As a workaround, when the uv coordinates are equal, we move the uv coordinates a little bit
                            if v0.uv == Pos2::ZERO && v1.uv == Pos2::ZERO && v2.uv == Pos2::ZERO {
                                v1.uv = Pos2::new(0.0, 1.0 / 65536.0);
                                v2.uv = Pos2::new(1.0 / 65536.0, 0.0);
                            }

                            push_vert(&v0);
                            push_vert(&v1);
                            push_vert(&v2);
                        }

                        let vertices =
                            Vertices::new_copy(VertexMode::Triangles, &pos, &texs, &colors, None);

                        arc.clip_rect(skclip_rect, ClipOp::default(), true);

                        let paint = &self.paints[&texture_id].paint;

                        arc.draw_vertices(&vertices, BlendMode::Modulate, paint);
                    }
                }
                Primitive::Callback(data) => {
                    let callback: Arc<EguiSkiaPaintCallback> = data.callback.downcast().unwrap();
                    let rect = data.rect;

                    let skia_rect = Rect::new(
                        rect.min.x * dpi,
                        rect.min.y * dpi,
                        rect.max.x * dpi,
                        rect.max.y * dpi,
                    );

                    let mut drawable: Drawable = callback.callback.deref()(skia_rect).0.unwrap();

                    let mut arc = skia_safe::AutoCanvasRestore::guard(canvas, true);

                    arc.clip_rect(skclip_rect, ClipOp::default(), true);
                    arc.translate((rect.min.x, rect.min.y));

                    drawable.draw(&mut arc, None);
                }
            }
        }

        textures_delta.free.iter().for_each(|id| {
            self.paints.remove(id);
        });
    }
}

pub struct EguiSkiaPaintCallback {
    callback: Box<dyn Fn(Rect) -> SyncSendableDrawable + Send + Sync>,
}

impl EguiSkiaPaintCallback {
    pub fn new<F: Fn(&mut Canvas) + Send + Sync + 'static>(callback: F) -> EguiSkiaPaintCallback {
        EguiSkiaPaintCallback {
            callback: Box::new(move |rect| {
                let mut pr = PictureRecorder::new();
                let mut canvas = pr.begin_recording(rect, None);
                callback(&mut canvas);
                SyncSendableDrawable(
                    pr.finish_recording_as_drawable()
                        .unwrap()
                        .wrap_send()
                        .unwrap(),
                )
            }),
        }
    }
}

struct SyncSendableDrawable(pub Sendable<Drawable>);

unsafe impl Sync for SyncSendableDrawable {}
