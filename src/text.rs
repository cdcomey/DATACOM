use wgpu::util::DeviceExt;
use image;
use log::debug;
use rusttype::{Font, Scale, point};
use std::collections::HashMap;
use std::sync::Arc;

/// Single character in the font. Has texture coordinates, size, bearing, and an advance.
pub struct Glyph {
    tex_coords: [f32; 4],           // tex_coords coordinates in texture atlas
    size: [f32; 2],         // Glyph width and height in pixels
    bearing: [f32; 2],      // Offset for positioning
    advance: f32,           // Horizontal advance after rendering
}

/// Function to load font atlas. Loads a given font. 
pub fn load_font_atlas(path: &str, font_size: f32) -> (image::RgbaImage, HashMap<char, Glyph>) {
    let font_data = std::fs::read(path).expect("Failed to read font file");
    let font = Font::try_from_vec(font_data).expect("Failed to load font");
    let glyph_spacing = 1;  // space the glyphs out a bit so there is no bleeding when sampled
    debug!("Font info: {:?}", font);

    let scale = Scale::uniform(font_size);

    let chars: Vec<char> = (' '..='~').collect(); // ASCII range
    let mut glyph_infos = HashMap::new();
    
    let mut max_height = 0;
    let mut total_width = 0;

    let glyphs: Vec<_> = chars.iter()
        .map(|&c| font.glyph(c).scaled(scale).positioned(point(0.0, 0.0)))
        .collect();

    for glyph in &glyphs {
        if let Some(bb) = glyph.pixel_bounding_box() {
            max_height = max_height.max(bb.height());
            total_width += bb.width() + glyph_spacing;
        }
    }

    let mut atlas = image::RgbaImage::new(total_width as u32, max_height as u32);
    let mut x_offset = 0;

    for (i, glyph) in glyphs.iter().enumerate() {
        if let Some(bb) = glyph.pixel_bounding_box() {
            
            let glyph_width = bb.width();
            let glyph_height = bb.height();
            // println!("'{}': min = ({}, {}), max = ({}, {}), width = {}, height = {}", chars[i], bb.min.x, bb.min.y, bb.max.x, bb.max.y, bb.width(), bb.height());

            let mut glyph_bitmap = image::RgbaImage::new(glyph_width as u32, glyph_height as u32);
            glyph.draw(|x, y, v| {
                let intensity = (v * 255.0) as u8;
                glyph_bitmap.put_pixel(x, y, image::Rgba([255, 255, 255, intensity]));
            });

            image::imageops::overlay(&mut atlas, &glyph_bitmap, x_offset as i64, 0);

            glyph_infos.insert(chars[i], Glyph {
                tex_coords: [
                    (x_offset + 0) as f32 / atlas.width() as f32,
                    0.0,
                    (x_offset + glyph_width - 0) as f32 / atlas.width() as f32,
                    glyph_height as f32 / atlas.height() as f32,
                ],
                size: [glyph_width as f32, glyph_height as f32],
                bearing: [bb.min.x as f32, bb.min.y as f32],
                advance: glyph.unpositioned().h_metrics().advance_width,
            });

            x_offset += glyph_width + glyph_spacing;
            debug!("Glyph: {}, Width: {}", chars[i], glyph_width)
        }
    }
    atlas.save("atlas_example.png").expect("Failed to save atlas");
    
    for (key, glyph) in &glyph_infos {
        debug!("{}, {:?}", key, glyph.size)
    }

    // Rust drops the space character for some reason so we're putting it back in here
    let space_glyph = font.glyph(' ').scaled(scale);
    let space_advance = space_glyph.h_metrics().advance_width;
    glyph_infos.insert(' ', Glyph {
        tex_coords: [0.0, 0.0, 0.0, 0.0],   // No texture needed
        size: [0.0, 0.0],                   // No size
        bearing: [0.0, 0.0],                // No bearing
        advance: space_advance,             // Correct spacing
    });
    
    (atlas, glyph_infos)
}

/// WGPU vertex struct, differs from main one in that it has texture coordinates
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GlyphVertex {
    position: [f32; 3],
    uv: [f32; 2],
    color: [f32; 4]
}

impl GlyphVertex {
    fn new(pos: [f32; 2], uv: [f32; 2], color: cgmath::Vector3<f32>) -> Self {
        GlyphVertex {
            position: [pos[0], pos[1], 0.0],
            uv,
            color: [color.x, color.y, color.z, 1.0],
        }
    }

    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        use std::mem;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<GlyphVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    offset: mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: mem::size_of::<[f32; 5]>() as wgpu::BufferAddress,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

/// Creates texture from font atlas for OpenGL
pub fn create_texture_atlas(
    device: &wgpu::Device, 
    queue: &wgpu::Queue, 
    format: &wgpu::TextureFormat,
    atlas: image::RgbaImage
) -> wgpu::Texture {

    let image_dimensions = atlas.dimensions();
    // println!("image dimensions: {}, {}", image_dimensions.0, image_dimensions.1);
    let raw_data = atlas.clone().into_raw(); // Convert to Vec<u8>
    // println!("raw data: {:?}", raw_data);

    // Validates data size 
    let expected_size = (image_dimensions.0 * image_dimensions.1 * 4) as usize; // 4 bytes per pixel (RGBA)
    assert_eq!(
        raw_data.len(),
        expected_size,
        "Texture data size mismatch: expected {} bytes, found {} bytes",
        expected_size,
        raw_data.len()
    );

    let texture = device.create_texture_with_data(
        queue, 
        &wgpu::TextureDescriptor {
            label: Some("Texture Atlas"),
            size: wgpu::Extent3d {
                width: image_dimensions.0, 
                height: image_dimensions.1, 
                depth_or_array_layers: 1
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: *format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        }, 
        wgpu::wgt::TextureDataOrder::LayerMajor, // not sure if this is correct 
        &raw_data[..],
    );

    texture
}

pub struct TextMesh {
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_elements: u32,
}

impl TextMesh {
    fn init_buffers(
        device: &wgpu::Device, 
        content: &String, 
        glyph_map: &Arc<HashMap<char, Glyph>>, 
        x_offset: f32, 
        y_offset: f32, 
        color: cgmath::Vector3<f32>, 
    ) -> Self {
        let mut vertices = vec![];
        let mut indices = vec![];
        let mut cursor_x = 0.0;
        // debug!("Drawing text");
        for (i, c) in content.chars().enumerate() {
            if let Some(glyph) = glyph_map.get(&c) {
                let x0 = x_offset + cursor_x + glyph.bearing[0];
                let y0 = y_offset + glyph.bearing[1];
                let x1 = x0 + glyph.size[0];
                let y1 = y0 + glyph.size[1];
                // println!("INFO FOR '{}'", c);
                // println!("x-offset = {}, y-offset = {}", x_offset, y_offset);
                // println!("h-bearing = {}, v-bearing = {}", glyph.bearing[0], glyph.bearing[1]);
                // println!("width = {}, height = {}", glyph.size[0], glyph.size[1]);
                // println!("coords: ({x0}, {y0}), ({x1}, {y0}), ({x1}, {y1}), ({x0}, {y1})");

                let tex_coords = glyph.tex_coords;
                // println!("tex coords: {:?}", tex_coords);
                let u0 = tex_coords[0];
                let v0 = tex_coords[1];
                let u1 = tex_coords[2];
                let v1 = tex_coords[3];
                vertices.push(GlyphVertex::new([x0, y0], [u0, v0], color));
                vertices.push(GlyphVertex::new([x1, y0], [u1, v0], color));
                vertices.push(GlyphVertex::new([x1, y1], [u1, v1], color));
                vertices.push(GlyphVertex::new([x0, y1], [u0, v1], color));
                // println!("vertex 1 UV: {}, {}", u0, v0);
                // println!("vertex 2 UV: {}, {}", u1, v0);
                // println!("vertex 3 UV: {}, {}", u1, v1);
                // println!("vertex 4 UV: {}, {}", u0, v1);
                // println!("Coords for glyph {i}: {x0}, {y0}, {x1}, {y1}");
                // let vec1 = cgmath::Vector2::new(x0, y0);
                // let vec2 = cgmath::Vector2::new(x1, y1);
                // println!("transform 1 = {:?}", ortho_transform_matrix * vec1);
                // println!("transform 2 = {:?}", ortho_transform_matrix * vec2);

                let base = (i * 4) as u16;
                indices.extend_from_slice(&[
                    base, base+1, base+2, 
                    base, base+2, base+3]);

                cursor_x += glyph.advance;
            }
        }

        let vertex_data = bytemuck::cast_slice(&vertices);
        let index_data = bytemuck::cast_slice(&indices);

        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Text Vertex Buffer"),
            contents: vertex_data,
            usage: wgpu::BufferUsages::VERTEX,
        });

        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Text Index Buffer"),
            contents: index_data,
            usage: wgpu::BufferUsages::INDEX,
        });

        let num_elements = indices.len() as u32;

        TextMesh {
            vertex_buffer,
            index_buffer,
            num_elements,
        }
    }
}

// Struct for DATACOM to display text.
pub struct TextDisplay {
    content: String,
    glyph_map: Arc<HashMap<char, Glyph>>,
    mesh: TextMesh,
    x_start: f32,
    y_start: f32,
    color: cgmath::Vector3<f32>,
    bind_group: wgpu::BindGroup,
}

impl TextDisplay {
    pub fn new(
        content: String, 
        glyph_map: Arc<HashMap<char, Glyph>>, 
        x_start: f32, 
        y_start: f32, 
        color: cgmath::Vector3<f32>,
        device: &wgpu::Device,
        texture_atlas: &wgpu::Texture,
        atlas_sampler: &wgpu::Sampler,
        bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let mesh = TextMesh::init_buffers(device, &content, &glyph_map, x_start, y_start, color);

        let texture_atlas_view = texture_atlas.create_view(&wgpu::TextureViewDescriptor::default());

        let text_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("TextDisplay Bind Group"),
            layout: bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                },
            ],
        });

        TextDisplay {
            content,
            glyph_map,
            x_start,
            y_start,
            color,
            mesh,
            bind_group: text_bind_group,
        }
    }

    /// Function to change text in string.
    pub fn change_text(&mut self, device: &wgpu::Device, new_string: String) {
        self.content = new_string;
        self.mesh = TextMesh::init_buffers(device, &self.content, &self.glyph_map, self.x_start, self.y_start, self.color);
    }

    pub fn draw<'a>(
        &'a self,
        ortho_matrix_bind_group: &'a wgpu::BindGroup,
        render_pass: &mut wgpu::RenderPass<'a>,
    ) {
        render_pass.draw_text(&self, ortho_matrix_bind_group, &self.bind_group);
    }
}

pub trait DrawText<'a> {
    fn draw_text(
        &mut self,
        text: &'a TextDisplay,
        ortho_matrix_bind_group: &'a wgpu::BindGroup,
        text_bind_group: &'a wgpu::BindGroup,
    );
}

impl<'a, 'b> DrawText<'b> for wgpu::RenderPass<'a> 
where 
    'b: 'a,
{
    fn draw_text(
        &mut self, 
        text: &'b TextDisplay,
        ortho_matrix_bind_group: &'b wgpu::BindGroup,
        text_bind_group: &'b wgpu::BindGroup,
    ) {
        self.set_vertex_buffer(0, text.mesh.vertex_buffer.slice(..));
        self.set_index_buffer(text.mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
        self.set_bind_group(0, ortho_matrix_bind_group, &[]);
        self.set_bind_group(1, text_bind_group, &[]);
        self.draw_indexed(0..text.mesh.num_elements, 0, 0..1);
    }
}

pub fn get_font() -> String{
    #[cfg(target_os="macos")]
    {
        "/Library/Fonts/Arial Unicode.ttf".to_string()
    }

    #[cfg(target_os="windows")]
    {
        "/usr/share/fonts/truetype/futura/JetBrainsMono-Bold.ttf".to_string()
    }

    #[cfg(target_os="linux")]
    {
        "/usr/share/fonts/truetype/futura/JetBrainsMono-Bold.ttf".to_string()
    }
}
