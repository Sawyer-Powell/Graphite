# Graphite's End-to-End Rendering Flow

This document traces the complete execution flow from animation frame to final canvas rendering in Graphite's architecture.

## Overview

Graphite uses a **reactive, cached, async rendering pipeline** where:
- Changes propagate through message passing
- Compilation happens on a separate thread  
- Only recompiles when the document actually changes
- User networks get wrapped in rendering infrastructure

## The Complete Flow

### 1. Animation Frame Trigger (60 FPS)

Every `requestAnimationFrame` (16.67ms), the browser triggers the render loop:

**File**: [`frontend/wasm/src/editor_api.rs`](frontend/wasm/src/editor_api.rs)
```rust
// In init_after_frontend_ready's setInterval loop
for message in editor.handle_message(AnimationMessage::IncrementFrameCounter) {
    handle.send_frontend_message_to_js(message);
}
```

### 2. Message Cascade (Reactive Updates)

The animation frame triggers a cascade of messages:

**File**: [`editor/src/messages/animation/animation_message_handler.rs`](editor/src/messages/animation/animation_message_handler.rs)
```rust
AnimationMessage::IncrementFrameCounter => {
    if self.is_playing() {
        self.frame_index += 1.;
        responses.add(AnimationMessage::UpdateTime);
    }
}
```

This flows through:
```
AnimationMessage::IncrementFrameCounter
→ AnimationMessage::UpdateTime  
→ PortfolioMessage::SubmitActiveGraphRender
→ PortfolioMessage::SubmitGraphRender
```

### 3. Node Graph Evaluation Request

**File**: [`editor/src/messages/portfolio/portfolio_message_handler.rs`](editor/src/messages/portfolio/portfolio_message_handler.rs)
```rust
PortfolioMessage::SubmitGraphRender { document_id, ignore_hash } => {
    let inspect_node = self.inspect_node_id();
    let result = self.executor.submit_node_graph_evaluation(
        self.documents.get_mut(&document_id).expect("Tried to render non-existent document"),
        ipp.viewport_bounds.size().as_uvec2(),
        timing_information,
        inspect_node,
        ignore_hash,
    );
}
```

This calls into the `NodeGraphExecutor`:

**File**: [`editor/src/node_graph_executor.rs`](editor/src/node_graph_executor.rs)
```rust
pub fn submit_node_graph_evaluation(
    &mut self,
    document: &mut DocumentMessageHandler,
    viewport_resolution: UVec2,
    time: TimingInformation,
    inspect_node: Option<NodeId>,
    ignore_hash: bool,
) -> Result<(), String> {
    self.update_node_graph(document, inspect_node, ignore_hash)?;  // ← Goes to section 4
    self.submit_current_node_graph_evaluation(document, viewport_resolution, time)?;  // ← Goes to section 5
    Ok(())
}
```

### 4. Smart Caching Check (update_node_graph)

**File**: [`editor/src/node_graph_executor.rs`](editor/src/node_graph_executor.rs)
```rust
fn update_node_graph(&mut self, document: &mut DocumentMessageHandler, inspect_node: Option<NodeId>, ignore_hash: bool) -> Result<(), String> {
    let network_hash = document.network_interface.document_network().current_hash();
    // Only recompile if the network actually changed
    if network_hash != self.node_graph_hash || self.old_inspect_node != inspect_node || ignore_hash {
        let network = document.network_interface.document_network().clone();
        self.old_inspect_node = inspect_node;
        self.node_graph_hash = network_hash;

        self.runtime_io
            .send(GraphRuntimeRequest::GraphUpdate(GraphUpdate { network, inspect_node })) // QUEUES A GRAPH UPDATE HERE
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
```

**Key insight**: The system only sends updates when the document actually changes!

After the caching check, the function continues to submit the execution request:

**File**: [`editor/src/node_graph_executor.rs`](editor/src/node_graph_executor.rs)
```rust
fn submit_current_node_graph_evaluation(&mut self, document: &mut DocumentMessageHandler, viewport_resolution: UVec2, time: TimingInformation) -> Result<(), String> {
    let render_config = RenderConfig {
        viewport: Footprint::new_with_transform(viewport_resolution, document.metadata().document_to_viewport),
        export_format: ExportFormat::Canvas,
        time: time.into(),
        view_mode: document.view_mode,
        hide_artboards: false,
        for_export: false,
    };
    
    self.queue_execution(render_config);  // ← Goes to section 5
    Ok(())
}
```

### 5. Execution Request (queue_execution)

The execution request is queued with the render configuration:

**File**: [`editor/src/node_graph_executor.rs`](editor/src/node_graph_executor.rs)
```rust
fn queue_execution(&self, render_config: RenderConfig) -> u64 {
    let execution_id = generate_uuid();
    let request = ExecutionRequest { execution_id, render_config };
    self.runtime_io.send(GraphRuntimeRequest::ExecutionRequest(request)).expect("Failed to send generation request");
    execution_id
}
```

### 6. Runtime Processing (Separate Thread)

Both the `GraphUpdate` (from section 4) and `ExecutionRequest` (from section 5) messages are processed by the `NodeRuntime` on a separate thread. 

#### Thread Setup

The runtime thread is set up differently depending on the platform:

**Native (Tauri)**: [`frontend/src-tauri/src/main.rs`](frontend/src-tauri/src/main.rs)
```rust
// Spawns a dedicated thread that polls the runtime every 16ms
std::thread::spawn(|| loop {
    futures::executor::block_on(graphite_editor::node_graph_executor::run_node_graph());
    std::thread::sleep(std::time::Duration::from_millis(16))
});
```

**WASM**: [`frontend/wasm/src/editor_api.rs`](frontend/wasm/src/editor_api.rs)
```rust
// Called on every requestAnimationFrame (60 FPS)
fn init_after_frontend_ready(&self, platform: String) {
    *g.borrow_mut() = Some(Closure::new(move |_timestamp| {
        wasm_bindgen_futures::spawn_local(poll_node_graph_evaluation());
        // ... animation frame handling
    }));
}

async fn poll_node_graph_evaluation() {
    if !editor::node_graph_executor::run_node_graph().await {
        return;
    };
    // ... process results
}
```

**Runtime Initialization**: [`editor/src/node_graph_executor/runtime_io.rs`](editor/src/node_graph_executor/runtime_io.rs)
```rust
pub fn new() -> Self {
    let (response_sender, response_receiver) = std::sync::mpsc::channel();
    let (request_sender, request_receiver) = std::sync::mpsc::channel();
    futures::executor::block_on(replace_node_runtime(NodeRuntime::new(request_receiver, response_sender)));

    Self {
        sender: request_sender,      // Send TO runtime
        receiver: response_receiver, // Receive FROM runtime
    }
}
```

#### Message Processing

The runtime polls for messages and handles them in order:

**File**: [`editor/src/node_graph_executor/runtime.rs`](editor/src/node_graph_executor/runtime.rs)

#### 6a. Graph Update Processing
```rust
GraphRuntimeRequest::GraphUpdate(GraphUpdate { mut network, inspect_node }) => {
    // Insert monitor nodes for inspection
    self.inspect_state = inspect_node.map(|inspect| InspectState::monitor_inspect_node(&mut network, inspect));

    self.old_graph = Some(network.clone());
    self.node_graph_errors.clear();
    let result = self.update_network(network).await;  // ← COMPILATION HAPPENS HERE
    self.update_thumbnails = true;
    self.sender.send_generation_response(CompilationResponse {
        result,
        node_graph_errors: self.node_graph_errors.clone(),
    });
}
```

#### 6b. Network Compilation
```rust
async fn update_network(&mut self, mut graph: NodeNetwork) -> Result<ResolvedDocumentNodeTypesDelta, String> {
    preprocessor::expand_network(&mut graph, &self.substitutions);

    let scoped_network = wrap_network_in_scope(graph, self.editor_api.clone());

    // We assume only one output
    assert_eq!(scoped_network.exports.len(), 1, "Graph with multiple outputs not yet handled");

    let c = Compiler {};
    let proto_network = match c.compile_single(scoped_network) {
        Ok(network) => network,
        Err(e) => return Err(e),
    };
    
    // Build executable nodes in the BorrowTree
    self.executor.update(proto_network).await.map_err(|e| {
        self.node_graph_errors.clone_from(&e);
        format!("{e:?}")
    })
}
```

#### 6c. Execution Request Processing
```rust
GraphRuntimeRequest::ExecutionRequest(ExecutionRequest { execution_id, render_config, .. }) => {
    let transform = render_config.viewport.transform;

    let result = self.execute_network(render_config).await;  // ← EXECUTION HAPPENS HERE
    let mut responses = VecDeque::new();
    self.process_monitor_nodes(&mut responses, self.update_thumbnails);
    self.update_thumbnails = false;

    let inspect_result = self.inspect_state.and_then(|state| state.access(&self.executor));

    self.sender.send_execution_response(ExecutionResponse {
        execution_id,
        result,
        responses,
        transform,
        vector_modify: self.vector_modify.clone(),
        inspect_result,
    });
}
```

### 7. Network Wrapping

The user's document network (initially just an Artboard node) gets wrapped in rendering infrastructure:

**File**: [`node-graph/interpreted-executor/src/util.rs`](node-graph/interpreted-executor/src/util.rs)
```rust
pub fn wrap_network_in_scope(mut network: NodeNetwork, editor_api: Arc<WasmEditorApi>) -> NodeNetwork {
    // Creates a 3-layer structure:
    // Layer 0: User's document network  
    // Layer 1: Rendering infrastructure (memo cache, GPU surface, render node)
    // Layer 2: Editor API injection (fonts, preferences, etc.)
    
    NodeNetwork {
        exports: vec![NodeInput::node(NodeId(1), 0)],  // Points to render pipeline
        nodes: nodes.into_iter().enumerate().map(|(id, node)| (NodeId(id as u64), node)).collect(),
        // ...
    }
}
```

### 8. Network Execution

**File**: [`editor/src/node_graph_executor/runtime.rs`](editor/src/node_graph_executor/runtime.rs)
```rust
async fn execute_network(&mut self, render_config: RenderConfig) -> Result<TaggedValue, String> {
    let result = match self.executor.input_type() {
        Some(t) if t == concrete!(RenderConfig) => (&self.executor).execute(render_config).await.map_err(|e| e.to_string()),
        Some(t) if t == concrete!(()) => (&self.executor).execute(()).await.map_err(|e| e.to_string()),
        Some(t) => Err(format!("Invalid input type {t:?}")),
        _ => Err(format!("No input type:\n{:?}", self.node_graph_errors)),
    };
    
    result.map_err(|e| e)?
}
```

### 9. Result Processing (Main Thread)

**File**: [`editor/src/node_graph_executor.rs`](editor/src/node_graph_executor.rs)
```rust
pub fn poll_node_graph_evaluation(&mut self, responses: &mut VecDeque<Message>) -> Result<(), String> {
    while let Ok(node_graph_update) = self.runtime_io.try_receive() {
        match node_graph_update {
            NodeGraphUpdate::ExecutionResponse(ExecutionResponse { result, transform, .. }) => {
                match result {
                    Ok(output) => {
                        self.process_node_graph_output(output, transform, responses)?
                    }
                    // Handle errors...
                }
            }
            // Handle other response types...
        }
    }
}
```

### 10. Final Frontend Update

**File**: [`editor/src/node_graph_executor.rs`](editor/src/node_graph_executor.rs)
```rust
fn process_node_graph_output(&mut self, node_graph_output: TaggedValue, transform: DAffine2, responses: &mut VecDeque<Message>) -> Result<(), String> {
    match node_graph_output {
        TaggedValue::RenderOutput(render_output) => {
            match render_output.data {
                RenderOutputType::Svg(svg) => {
                    responses.add(FrontendMessage::UpdateDocumentArtwork { svg });
                }
                RenderOutputType::CanvasFrame(frame) => {
                    let svg = format!(
                        r#"<svg><foreignObject width="{}" height="{}"><div data-canvas-placeholder="canvas{}"></div></foreignObject></svg>"#,
                        frame.resolution.x, frame.resolution.y, frame.surface_id.0
                    );
                    responses.add(FrontendMessage::UpdateDocumentArtwork { svg });
                }
            }
        }
    }
}
```

## Key Components

### Message Types
- **`AnimationMessage`**: Animation frame and time updates
- **`PortfolioMessage`**: Document-level operations  
- **`GraphRuntimeRequest`**: Requests to the runtime thread
- **`NodeGraphUpdate`**: Responses from the runtime thread
- **`FrontendMessage`**: Updates to the browser frontend

### Core Structures
- **`DocumentNode`**: High-level user representation
- **`ProtoNode`**: Intermediate compilation representation  
- **`SharedNodeContainer`**: Runtime executable representation
- **`TaggedValue`**: Type-erased results (RenderOutput, SVG, etc.)

### Thread Architecture
- **Main Thread**: Owns documents, handles UI, processes results
- **Runtime Thread**: Compiles and executes node graphs
- **Communication**: Async channels with message passing

## Performance Optimizations

1. **Hash-based Caching**: Only recompiles when network changes
2. **Async Compilation**: Non-blocking on separate thread
3. **Memo Nodes**: Cache expensive computations  
4. **Monitor Nodes**: Generate thumbnails and debug info
5. **Type Erasure**: Runtime flexibility with compile-time safety

This architecture enables Graphite to achieve 60 FPS rendering while maintaining the flexibility of a node-based editor.