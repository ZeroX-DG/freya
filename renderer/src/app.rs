use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    task::Waker,
};

use dioxus_core::{Template, VirtualDom};
use dioxus_native_core::SendAnyMap;
use freya_common::{EventMessage, LayoutNotifier};
use freya_core::{
    dom::DioxusSafeDOM,
    events::{DomEvent, EventsProcessor, FreyaEvent},
    process_events, EventEmitter, EventReceiver, EventsQueue, ViewportsCollection,
};
use freya_layout::Layers;
use futures::FutureExt;
use futures::{
    pin_mut,
    task::{self, ArcWake},
};
use tokio::{
    select,
    sync::mpsc::{unbounded_channel, UnboundedSender},
};
use winit::{dpi::PhysicalSize, event_loop::EventLoopProxy};

use crate::{HoveredNode, WindowEnv};

pub fn winit_waker(proxy: &EventLoopProxy<EventMessage>) -> std::task::Waker {
    struct DomHandle(EventLoopProxy<EventMessage>);

    unsafe impl Send for DomHandle {}
    unsafe impl Sync for DomHandle {}

    impl ArcWake for DomHandle {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            _ = arc_self.0.send_event(EventMessage::PollVDOM);
        }
    }

    task::waker(Arc::new(DomHandle(proxy.clone())))
}

/// Manages the Application lifecycle
pub struct App<State: 'static + Clone> {
    rdom: DioxusSafeDOM,
    vdom: VirtualDom,

    events: EventsQueue,

    vdom_waker: Waker,
    proxy: EventLoopProxy<EventMessage>,
    mutations_sender: Option<UnboundedSender<()>>,

    event_emitter: EventEmitter,
    event_receiver: EventReceiver,

    window_env: WindowEnv<State>,

    layers: Layers,
    events_processor: EventsProcessor,
    viewports_collection: ViewportsCollection,
    layout_notifier: LayoutNotifier,
}

impl<State: 'static + Clone> App<State> {
    pub fn new(
        rdom: DioxusSafeDOM,
        vdom: VirtualDom,
        proxy: &EventLoopProxy<EventMessage>,
        mutations_sender: Option<UnboundedSender<()>>,
        window_env: WindowEnv<State>,
    ) -> Self {
        let (event_emitter, event_receiver) = unbounded_channel::<DomEvent>();
        Self {
            rdom,
            vdom,
            events: Vec::new(),
            vdom_waker: winit_waker(proxy),
            proxy: proxy.clone(),
            mutations_sender,
            event_emitter,
            event_receiver,
            window_env,
            layers: Layers::default(),
            events_processor: EventsProcessor::default(),
            viewports_collection: HashMap::default(),
            layout_notifier: Arc::new(Mutex::new(false)),
        }
    }

    /// Provide the launch state and few other utilities like the EventLoopProxy
    pub fn provide_vdom_contexts(&self) {
        if let Some(state) = self.window_env.window_config.state.clone() {
            self.vdom.base_scope().provide_context(state);
        }
        self.vdom.base_scope().provide_context(self.proxy.clone());
    }

    /// Make an first build of the VirtualDOM
    pub fn init_vdom(&mut self) {
        self.provide_vdom_contexts();

        let mutations = self.vdom.rebuild();
        let (to_update, diff) = self.rdom.dom_mut().apply_mutations(mutations);

        if !diff.is_empty() {
            self.mutations_sender.as_ref().map(|s| s.send(()));
        }

        *self.layout_notifier.lock().unwrap() = false;

        let mut ctx = SendAnyMap::new();
        ctx.insert(self.layout_notifier.clone());

        self.rdom.dom_mut().update_state(to_update, ctx);
    }

    /// Update the RealDOM with changes from the VirtualDOM
    pub fn apply_vdom_changes(&mut self) -> (bool, bool) {
        let mutations = self.vdom.render_immediate();
        let (to_update, diff) = self.rdom.dom_mut().apply_mutations(mutations);

        if !diff.is_empty() {
            self.mutations_sender.as_ref().map(|s| s.send(()));
        }

        *self.layout_notifier.lock().unwrap() = false;

        let mut ctx = SendAnyMap::new();
        ctx.insert(self.layout_notifier.clone());

        self.rdom.dom_mut().update_state(to_update, ctx);

        (!diff.is_empty(), *self.layout_notifier.lock().unwrap())
    }

    /// Poll the VirtualDOM for any new change
    pub fn poll_vdom(&mut self) {
        let waker = &self.vdom_waker.clone();
        let mut cx = std::task::Context::from_waker(waker);

        loop {
            self.provide_vdom_contexts();

            {
                let fut = async {
                    select! {
                        ev = self.event_receiver.recv() => {
                            if let Some(ev) = ev {
                                let data = ev.data.any();
                                self.vdom.handle_event(&ev.name, data, ev.element_id, false);

                                self.vdom.process_events();
                            }
                        },
                        _ = self.vdom.wait_for_work() => {},
                    }
                };
                pin_mut!(fut);

                match fut.poll_unpin(&mut cx) {
                    std::task::Poll::Ready(_) => {}
                    std::task::Poll::Pending => break,
                }
            }

            let (must_repaint, must_relayout) = self.apply_vdom_changes();
            // TODO: Temp fix, I should probably handle the incremental mutations myself.
            if must_relayout || must_repaint {
                self.request_redraw();
            } else if must_repaint {
                self.request_rerender();
            }
        }
    }

    /// Process the events queue
    pub fn process_events(&mut self) {
        process_events(
            &self.rdom.dom(),
            &self.layers,
            &mut self.events,
            &self.event_emitter,
            &mut self.events_processor,
            &self.viewports_collection,
        )
    }

    /// Measure the layout
    pub fn process_layout(&mut self) {
        let (layers, viewports) = self.window_env.process_layout(&self.rdom.dom());
        self.layers = layers;
        self.viewports_collection = viewports;
    }

    /// Push an event to the events queue
    pub fn push_event(&mut self, event: FreyaEvent) {
        self.events.push(event);
    }

    /// Request a redraw
    pub fn request_redraw(&self) {
        self.window_env.request_redraw();
    }

    /// Request a rerender
    pub fn request_rerender(&self) {
        self.proxy
            .send_event(EventMessage::RequestRerender)
            .unwrap();
    }

    /// Replace a VirtualDOM Template
    pub fn vdom_replace_template(&mut self, template: Template<'static>) {
        self.vdom.replace_template(template);
    }

    /// Render the RealDOM into the Window
    pub fn render(&mut self, hovered_node: &HoveredNode) {
        self.window_env.render(
            &self.layers,
            &self.viewports_collection,
            hovered_node,
            &self.rdom.dom(),
        );
    }

    /// Resize the Window
    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        self.window_env.resize(size);
    }
}
