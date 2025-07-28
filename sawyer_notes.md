# Graphite Rendering System Analysis

## Overview

Graphite is a procedural 2D graphics editor built in Rust with a node-based architecture. The rendering system is sophisticated, supporting both SVG generation and hardware-accelerated rendering through multiple backends.

## Architecture Components

### Core Rendering Abstraction

The `GraphicElementRendered` trait is the central abstraction for all renderable elements:

- **render_svg()** - Generates SVG markup
- **render_to_vello()** - Hardware-accelerated rendering via Vello/WebGPU  
- **collect_metadata()** - Gathers interaction data (click targets, transforms)
- **add_upstream_click_targets()** - Builds click detection system

### Dual Rendering Backends

**1. SVG Rendering (Primary)**
- Default rendering backend
- Generates scalable vector graphics as SVG strings
- Handles complex features: transforms, opacity, blend modes, clipping
- Used for display, export, and fallback scenarios
- Located in `node-graph/gsvg-renderer/`

**2. Vello Rendering (Hardware-Accelerated)**
- Optional GPU-accelerated rendering via Vello 2D graphics library
- Requires WebGPU support in browser
- Experimental feature toggleable in preferences
- Falls back to SVG when WebGPU unavailable
- Provides better performance for complex scenes

### Graphics Element Types

The system renders four main element types:

- **VectorData** - Paths, shapes, bezier curves
- **RasterDataCPU** - CPU-processed images  
- **RasterDataGPU** - GPU-processed images
- **GraphicGroup** - Collections/layers of elements
- **Artboard** - Canvas/document boundaries

## Rendering Pipeline

### 1. Node Graph Evaluation

The document is represented as a node graph that evaluates to produce `RenderOutput`:

```rust
RenderOutput {
    data: RenderOutputType::Svg(svg_string) | RenderOutputType::CanvasFrame(frame),
    metadata: RenderMetadata
}
```

### 2. Backend Selection

Runtime chooses rendering backend based on:
- User preferences (`use_vello` setting)
- WebGPU availability 
- Export requirements
- Fallback scenarios

### 3. SVG Generation Process

For SVG rendering:
1. Creates `SvgRender` instance to accumulate markup
2. Recursively calls `render_svg()` on graphic elements
3. Builds hierarchical SVG with proper transforms and styles
4. Handles advanced features like clipping paths and masks
5. Formats final SVG with viewBox and dimensions

### 4. Vello Rendering Process  

For hardware-accelerated rendering:
1. Creates Vello `Scene` object
2. Calls `render_to_vello()` to populate scene graph
3. Renders scene to WebGPU surface
4. Returns `CanvasFrame` with surface reference
5. Frontend displays via canvas placeholder

### 5. Frontend Integration

Rendered output flows to frontend:
- SVG strings displayed directly in DOM
- Canvas frames create placeholder elements  
- WASM interface handles user interactions
- Metadata enables click detection and selection

## Key Features

### Advanced Graphics Support
- **Transformations** - Matrix transforms, rotation, scaling
- **Blending** - Multiple blend modes (multiply, screen, etc.)
- **Opacity** - Alpha blending and transparency
- **Clipping** - Complex clipping paths and masks
- **Gradients** - Linear and radial gradients
- **Text** - Typography with font loading

### Performance Optimizations
- **Culling** - Viewport-based element culling
- **Caching** - Thumbnail and render caching
- **Lazy Evaluation** - Node graph lazy execution
- **GPU Acceleration** - Optional Vello backend

### Export Capabilities
- **Multiple Formats** - SVG, PNG, JPEG, etc.
- **Quality Settings** - Configurable export resolution
- **Background Options** - Transparent or colored backgrounds
- **Batch Export** - Multiple artboard export

### Interactive Features
- **Click Targets** - Precise layer selection
- **Bounding Boxes** - Element bounds calculation  
- **Transform Handles** - Visual manipulation widgets
- **Live Preview** - Real-time render updates

## File Structure

```
node-graph/gsvg-renderer/    # SVG rendering implementation
├── src/renderer.rs          # Main rendering logic
├── src/convert_usvg_path.rs # Path conversion utilities  
├── src/render_ext.rs        # Rendering extensions
└── src/to_peniko.rs         # Vello integration

editor/src/node_graph_executor.rs  # Rendering orchestration
frontend/src/                      # TypeScript frontend
├── utility-functions/rasterization.ts  # SVG to image conversion
└── io-managers/input.ts             # Canvas interaction
```

## Configuration

### User Preferences
- **Vello Renderer** - Enable/disable GPU acceleration
- **Vector Meshes** - Show/hide vector wireframes  
- **View Modes** - Normal, outline, etc.

### Render Parameters
- **ViewMode** - Normal, outline, export modes
- **Culling Bounds** - Viewport frustum culling
- **Export Settings** - Resolution, format, quality
- **Mask/Clip Flags** - Special rendering modes

## Technical Implementation Notes

### Memory Management
- Uses Rust's ownership system for safe memory handling
- WASM boundary managed via `wasm-bindgen`
- Image data shared between CPU/GPU contexts

### Error Handling  
- Graceful degradation when WebGPU unavailable
- SVG fallback for unsupported features
- Panic recovery with error visualization

### Browser Compatibility
- WebGPU detection and feature flags
- Canvas 2D fallback rendering  
- Progressive enhancement approach

### Performance Considerations
- Lazy node evaluation reduces unnecessary work
- Viewport culling eliminates off-screen rendering
- GPU acceleration for complex scenes
- Thumbnail caching for layer panels

## Future Improvements

- **WGPU Integration** - Direct WGPU rendering without Vello
- **Render Caching** - More aggressive caching strategies  
- **Multi-threading** - Parallel rendering workloads
- **Format Support** - Additional export formats
- **Animation** - Time-based rendering pipeline