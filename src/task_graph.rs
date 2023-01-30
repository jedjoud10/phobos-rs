use std::collections::HashMap;
/// # Task graph system
///
/// The task graph system exposes a powerful system for managing gpu-gpu synchronization of resources.
/// Note that the task graph deals in purely virtual resources. There are no physical resources bound to the task graph.

use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use ash::vk;

use petgraph::graph::*;
use petgraph;
use petgraph::Direction;
use petgraph::dot::Dot;
use petgraph::prelude::EdgeRef;
use petgraph::visit::{IntoEdgeReferences, IntoNodeReferences};
use crate::domain::ExecutionDomain;

use crate::error::Error;
use crate::IncompleteCommandBuffer;
use crate::pass::Pass;
use crate::pipeline::PipelineStage;

// Current issues:
// - If there is a barrier node with two dependent nodes, but both use the resource in a different way (e.g. layout), we should split this barrier in two barrier nodes and then serialize it.
//      => solving this should probably be a responsibility of the translation layer.

pub trait Resource {
    fn is_dependency_of(&self, lhs: &Self) -> bool;
    fn uid(&self) -> &String;
}

/// Task in a task dependency graph. This is parametrized on a resource type.
pub trait Task<R> where R: Resource {
    fn inputs(&self) -> &Vec<R>;
    fn outputs(&self) -> &Vec<R>;
}

/// Represents a barrier in the task graph.
pub trait Barrier<R> where R: Resource {
    fn new(resource: R) -> Self;
    fn resource(&self) -> &R;
}

#[derive(Debug, Clone)]
pub enum Node<R, B, T> where R: Resource, B: Barrier<R> + Clone, T: Task<R> + Clone {
    Task(T),
    Barrier(B),
    _Unreachable((!, PhantomData<R>)),
}

pub struct TaskGraph<R, B, T> where R: Resource + Default, B: Barrier<R> + Clone, T: Task<R> + Clone {
    pub(crate) graph: Graph<Node<R, B, T>, String>,
}


/// Represents a virtual resource in the system, uniquely identified by a string.
#[derive(Debug, Default, Clone)]
pub struct VirtualResource {
    pub uid: String,
}

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub enum ResourceUsage {
    #[default]
    Nothing,
    Present,
    Attachment,
    ShaderRead,
    ShaderWrite,
}

#[derive(Default, Debug, Clone)]
pub struct GpuResource {
    pub usage: ResourceUsage,
    pub resource: VirtualResource,
    pub stage: PipelineStage
}

#[derive(Debug, Clone)]
pub struct GpuBarrier<R = GpuResource> {
    pub resource: R,
    pub src_access: vk::AccessFlags2,
    pub dst_access: vk::AccessFlags2,
    pub src_stage: PipelineStage,
    pub dst_stage: PipelineStage,
}

pub struct GpuTask<R, D> where R: Resource, D: ExecutionDomain {
    pub identifier: String,
    pub inputs: Vec<R>,
    pub outputs: Vec<R>,
    pub execute: fn(&IncompleteCommandBuffer<D>) -> ()
}

pub struct GpuTaskGraph<D> where D: ExecutionDomain {
    graph: TaskGraph<GpuResource, GpuBarrier, GpuTask<GpuResource, D>>,
    // Note that this is guaranteed to be stable.
    // This is because the only time indices are invalidated is when deleting a node, and even then only the last
    // index is invalidated. Since the source is always the first node, this is never invalidated.
    source: NodeIndex,
}

impl VirtualResource {
    pub fn new(uid: String) -> Self {
        VirtualResource { uid }
    }

    /// 'Upgrades' the resource to a new version of itself. This is used to obtain the virtual resource name of an input resource after
    /// a task completes.
    pub fn upgrade(&self) -> Self {
        VirtualResource {
            uid: self.uid.clone() + "+"
        }
    }

    /// Returns the full, original name of the resource (without potential version star symbols)
    pub fn name(&self) -> String {
        let mut name = self.uid.clone();
        name.retain(|c| c != '+');
        name
    }

    pub fn is_source(&self) -> bool {
        // ends_with is a bit more efficient, since we know the '+' is always at the end of a resource uid.
        !self.uid.ends_with('+')
    }

    /// Two virtual resources are associated if and only if their uid's only differ by "*" symbols.
    pub fn are_associated(lhs: &VirtualResource, rhs: &VirtualResource) -> bool {
        // Since virtual resource uid's are constructed by appending * symbols, we can simply check whether the largest of the two strings starts with the shorter one
        let larger = if lhs.uid.len() >= rhs.uid.len() { lhs } else { rhs };
        let smaller = if lhs.uid.len() < rhs.uid.len() { lhs } else { rhs };
        larger.uid.starts_with(&smaller.uid)
    }

    /// One virtual resource is older than another if it has less '*' symbols.
    pub fn is_older(lhs: &VirtualResource, rhs: &VirtualResource) -> bool {
        if !VirtualResource::are_associated(&lhs, &rhs) { return false; }
        lhs.uid.len() < rhs.uid.len()
    }

    /// Note that this is not the same as inverting the result of as_older(), for the same exact state of the resource,
    /// both of these functions should return false (they decide whether resources are strictly older or younger than each other).
    pub fn is_younger(lhs: &VirtualResource, rhs: &VirtualResource) -> bool {
        if !VirtualResource::are_associated(&lhs, &rhs) { return false; }
        rhs.uid.len() < lhs.uid.len()
    }
}

impl GpuResource {
    pub fn virtual_resource(&self) -> &VirtualResource {
        &self.resource
    }
}

impl<> Barrier<GpuResource> for GpuBarrier {
    fn new(resource: GpuResource) -> Self {
        Self {
            src_access: resource.usage.access(),
            dst_access: vk::AccessFlags2::NONE,
            src_stage: resource.stage.clone(),
            dst_stage: PipelineStage::NONE,
            resource,
        }
    }

    fn resource(&self) -> &GpuResource {
        &self.resource
    }
}

impl Resource for GpuResource {
    fn is_dependency_of(&self, lhs: &Self) -> bool {
        self.virtual_resource().uid == lhs.virtual_resource().uid
    }

    fn uid(&self) -> &String {
        &self.virtual_resource().uid
    }
}

impl<R, D> Task<R> for GpuTask<R, D> where R: Resource, D: ExecutionDomain {
    fn inputs(&self) -> &Vec<R> {
        &self.inputs
    }

    fn outputs(&self) -> &Vec<R> {
        &self.outputs
    }
}

impl<R, D> Clone for GpuTask<R, D> where R: Resource + Clone, D: ExecutionDomain {
    fn clone(&self) -> Self {
        Self {
            identifier: self.identifier.clone(),
            inputs: self.inputs.clone(),
            outputs: self.outputs.clone(),
            execute: self.execute.clone()
        }
    }
}

impl ResourceUsage {
    pub fn access(&self) -> vk::AccessFlags2 {
        match self {
            ResourceUsage::Nothing => { vk::AccessFlags2::NONE }
            ResourceUsage::Present => { vk::AccessFlags2::NONE }
            ResourceUsage::Attachment => { vk::AccessFlags2::COLOR_ATTACHMENT_WRITE }
            ResourceUsage::ShaderRead => { vk::AccessFlags2::SHADER_READ }
            ResourceUsage::ShaderWrite => { vk::AccessFlags2::SHADER_WRITE }
        }
    }

    pub fn is_read(&self) -> bool {
        match self {
            ResourceUsage::Nothing => { true }
            ResourceUsage::Present => { false }
            ResourceUsage::Attachment => { false }
            ResourceUsage::ShaderRead => { true }
            ResourceUsage::ShaderWrite => { false }
        }
    }
}

impl<D> GpuTaskGraph<D> where D: ExecutionDomain {
    /// Create a new task graph.
    pub fn new() -> Self {
        let mut graph = GpuTaskGraph {
            graph: TaskGraph::new(),
            source: NodeIndex::default()
        };

        // insert dummy 'source' node. This node produces all initial inputs and is used for start of frame sync.
        graph.graph.add_task(GpuTask {
            identifier: "_source".to_string(),
            inputs: vec![],
            outputs: vec![],
            execute: |_| {}
        }).unwrap();
        // ...
        graph.source = graph.graph.graph.node_indices().next().unwrap();
        graph
    }

    pub fn add_pass(&mut self, pass: Pass<D>) -> Result<(), Error> {
        // Before adding this pass, we need to add every initial input (one with no '+' signs in its uid) to the output of the source node.
        let Node::Task(source) = self.graph.graph.node_weight_mut(self.source).unwrap() else { panic!("Graph does not have a source node"); };
        for input in &pass.inputs {
            if input.resource.is_source() {
                source.outputs.push(
                    GpuResource {
                        usage: ResourceUsage::Nothing,
                        resource: input.resource.clone(),
                        stage: PipelineStage::TOP_OF_PIPE,
                    }
                )
            }
        }

        self.graph.add_task(GpuTask {
            identifier: pass.name,
            inputs: pass.inputs,
            outputs: pass.outputs,
            execute: pass.execute
        })?;

        Ok(())
    }

    /// Builds the task graph so it can be recorded into a command buffer.
    pub fn build(&mut self) -> Result<(), Error> {
        self.graph.create_barrier_nodes();
        self.merge_identical_barriers()?;

        Ok(())
    }

    /// Returns the task graph built by the GPU task graph system, useful for outputting dotfiles.
    pub fn task_graph(&self) -> &TaskGraph<GpuResource, GpuBarrier, GpuTask<GpuResource, D>> {
        &self.graph
    }

    pub fn num_nodes(&self) -> usize {
        self.graph.graph.node_count()
    }

    pub(crate) fn source(&self) -> NodeIndex {
        self.source
    }

    fn barrier_src_resource(graph: &Graph<Node<GpuResource, GpuBarrier, GpuTask<GpuResource, D>>, String>, node: NodeIndex) -> Result<&GpuResource, Error> {
        let Node::Barrier(barrier) = graph.node_weight(node).unwrap() else { return Err(Error::NodeNotFound) };
        let edge = graph.edges_directed(node, Direction::Incoming).next().unwrap();
        let src_node = edge.source();
        // An edge from a barrier always points to a task.
        let Node::Task(task) = graph.node_weight(src_node).unwrap() else { unimplemented!() };
        // This unwrap() cannot fail, or the graph was constructed incorrectly.
        Ok(task.inputs.iter().find(|&input| input.uid() == barrier.resource.uid()).unwrap())
    }

    fn barrier_dst_resource(graph: &Graph<Node<GpuResource, GpuBarrier, GpuTask<GpuResource, D>>, String>, node: NodeIndex) -> Result<&GpuResource, Error> {
        // We know that:
        // 1) Each barrier has at least one outgoing edge
        // 2) During the merge, each outgoing edge from a barrier will have the same resource usage
        // Knowing this, we can simply pick the first edge in the list to determine the resource usage
        let Node::Barrier(barrier) = graph.node_weight(node).unwrap() else { return Err(Error::NodeNotFound) };
        let edge = graph.edges(node).next().unwrap();
        let dst_node = edge.target();
        // An edge from a barrier always points to a task.
        let Node::Task(task) = graph.node_weight(dst_node).unwrap() else { unimplemented!() };
        // This unwrap() cannot fail, or the graph was constructed incorrectly.
        Ok(task.inputs.iter().find(|&input| input.uid() == barrier.resource.uid()).unwrap())
    }

    fn barriers(graph: &Graph<Node<GpuResource, GpuBarrier, GpuTask<GpuResource, D>>, String>) -> impl Iterator<Item = (NodeIndex, &GpuBarrier)> + '_ {
        graph.node_indices().filter_map(|node| match graph.node_weight(node).unwrap() {
            Node::Task(_) => { None }
            Node::Barrier(barrier) => { Some((node, barrier)) }
            Node::_Unreachable(_) => { unreachable!() }
        })
    }

    // Pass in the build step where identical barriers are merged into one for efficiency reasons.
    fn merge_identical_barriers(&mut self) -> Result<(), Error> {
        let graph = &mut self.graph.graph;
        // Find a barrier that has duplicates
        let mut to_remove = Vec::new();
        let mut edges_to_add = Vec::new();
        let mut barrier_flags: HashMap<NodeIndex, _> = HashMap::new();

        for (node, barrier) in Self::barriers(&graph) {
            let dst_resource = &Self::barrier_dst_resource(&graph, node)?;
            let dst_usage = dst_resource.usage.clone();
            barrier_flags.insert(node, (dst_resource.stage.clone(), dst_usage.access()));
            // Now we know the usage of this barrier, we can find all other barriers with the exact same resource usage and
            // merge those with this one
            for (other_node, other_barrier) in Self::barriers(&graph) {
                if other_node == node { continue; }
                if to_remove.contains(&node) { continue; }
                let other_resource = Self::barrier_dst_resource(&graph, other_node)?;
                let other_usage = &other_resource.usage;
                if other_barrier.resource.uid() == barrier.resource.uid() {
                    if !other_usage.is_read() && !dst_usage.is_read() && other_usage != &dst_usage {
                        return Err(Error::IllegalTaskGraph);
                    }
                    to_remove.push(other_node);
                    edges_to_add.push((node, graph.edges(other_node).next().unwrap().target(), other_resource.uid().clone()));
                    let (stage, access) = barrier_flags.get(&node).cloned().unwrap();
                    barrier_flags.insert(node, (other_resource.stage | stage, other_resource.usage.access() | access));
                }
            }
        }

        for (src, dst, uid) in edges_to_add {
            graph.update_edge(src, dst, uid);
        }
        for node in graph.node_indices() {
            if let Node::Barrier(barrier) = graph.node_weight_mut(node).unwrap() {
                let (stage, access) = barrier_flags.get(&node).cloned().unwrap();
                barrier.dst_stage = stage;
                barrier.dst_access = access;
            }
        }
        graph.retain_nodes(|_, node| { !to_remove.contains(&node) });

        Ok(())
    }
}

impl<R, B, T> TaskGraph<R, B, T> where R: Debug + Clone + Default + Resource, B: Barrier<R> + Clone, T: Task<R> + Clone {
    pub fn new() -> Self {
        TaskGraph {
            graph: Graph::new()
        }
    }

    fn is_dependent(&self, graph: &Graph<Node<R, B, T>, String>, child: NodeIndex, parent: NodeIndex) -> Result<Option<R>, Error> {
        let child = graph.node_weight(child).ok_or(Error::NodeNotFound)?;
        let parent = graph.node_weight(parent).ok_or(Error::NodeNotFound)?;
        if let Node::Task(child) = child {
            if let Node::Task(parent) = parent {
                return Ok(child.inputs().iter().find(|&input| {
                    parent.outputs().iter().any(|output| input.is_dependency_of(&output))
                })
                .cloned());
            }
        }

        Ok(None)
    }

    fn is_task_node(graph: &Graph<Node<R, B, T>, String>, node: NodeIndex) -> Result<bool, Error> {
        Ok(matches!(graph.node_weight(node).ok_or(Error::NodeNotFound)?, Node::Task(_)))
    }

    fn get_edge_attributes(_: &Graph<Node<R, B, T>, String>, _: EdgeReference<String>) -> String {
        String::from("")
    }

    fn get_node_attributes(_: &Graph<Node<R, B, T>, String>, node: (NodeIndex, &Node<R, B, T>)) -> String {
        match node.1 {
            Node::Task(_) => { String::from("fillcolor = \"#5e6df7\"") }
            Node::Barrier(_) => { String::from("fillcolor = \"#f75e70\" shape=box") }
            Node::_Unreachable(_) => { unreachable!() }
        }
    }

    /// Add a task to the task graph.
    pub fn add_task(&mut self, task: T) -> Result<(), Error> {
        let node = self.graph.add_node(Node::Task(task));
        // When adding a node, we need to update edges in the graph.
        // X = The newly added node
        // For every node Y:
        //      1. If Y produces an output used by X
        //          Add a connection Y -> X
        //      2. If Y consumes an input produced by X
        //          Add a connection X -> Y
        // Check for cycles in the graph. If there is a cycle, adding this node results in an illegal state.

        // Note that we unwrap here as this must never fail.
        self.graph.node_indices().for_each(|other_node| {
            // task depends on other task, add an edge other_task -> task
            if let Some(dependency) = self.is_dependent(&self.graph, node, other_node).unwrap() {
                self.graph.add_edge(other_node, node, dependency.uid().clone());
            }

            // Note: no else here, since we will detect cycles and error on them,
            // which is better than silently ignoring some cycles.
            if let Some(dependency) = self.is_dependent(&self.graph, other_node, node).unwrap() {
                self.graph.add_edge(node, other_node, dependency.uid().clone());
            }
        });

        match petgraph::algo::is_cyclic_directed(&self.graph) {
            true => Err(Error::GraphHasCycle),
            false => Ok(())
        }
    }

    /// Create a maximum set of barrier nodes for the task graph. This means that we will assume every resource that is being consumed needs its own barrier.
    /// These barriers are not yet serialized, as we only want to do that after we know which barriers are equivalent.
    pub fn create_barrier_nodes(&mut self) {
        // We create barrier nodes as follows:
        // For each task node P:
        //      - For each resource R that P produces:
        //          - If there are no nodes that depend directly on this resource R, do nothing.
        //          - Otherwise, add a new barrier node B, acting on the resource R.
        //          - Then add an edge from P to B, and edges from B to each node Q that consumes the resource R directly.
        //          - Finally, remove the edges from P to each node Q.
        //
        // Note that this algorithm creates too many barriers for practical usage.
        // We will compact the amount of dependency barriers when translating this graph to a render graph

        self.graph.node_indices().clone().for_each(|node| {
            if !Self::is_task_node(&self.graph, node).unwrap() { return; }

            let Node::Task(task) = self.graph.node_weight(node).cloned().unwrap() else { unimplemented!() };
            for resource in task.outputs() {
                // Find all nodes in the graph that depend directly on this resource
                let consumers = self.graph.node_indices().filter(|&consumer| -> bool {
                    let consumer = self.graph.node_weight(consumer).unwrap();
                    match consumer {
                        Node::Task(t) => { t.inputs().iter().any(|input| input.is_dependency_of(&resource)) }
                        Node::Barrier(_) => false,

                        Node::_Unreachable(_) => { unreachable!() }
                    }
                }).collect::<Vec<NodeIndex>>();

                if consumers.is_empty() { return; }
                for consumer in consumers {
                    let barrier = self.graph.add_node(Node::Barrier(B::new(resource.clone())));
                    self.graph.update_edge(node, barrier, resource.uid().clone());
                    self.graph.update_edge(barrier, consumer, resource.uid().clone());
                    if let Some(edge) = self.graph.find_edge(node, consumer) {
                        self.graph.remove_edge(edge);
                    }
                }
            }
        })
    }
}

pub trait GraphViz {
    fn dot(&self) -> Result<String, Error>;
}

impl<D> GraphViz for TaskGraph<GpuResource, GpuBarrier<GpuResource>, GpuTask<GpuResource, D>> where D: ExecutionDomain {
    fn dot(&self) -> Result<String, Error> {
        Ok(format!("{}", Dot::with_attr_getters(&self.graph, &[], &Self::get_edge_attributes, &Self::get_node_attributes)))
    }
}

impl<D> Display for Node<GpuResource, GpuBarrier, GpuTask<GpuResource, D>> where D: ExecutionDomain {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Node::Task(task) => f.write_fmt(format_args!("Task: {}", &task.identifier)),
            Node::Barrier(barrier) => { f.write_fmt(format_args!("{}({:#?} => {:#?})\n({:#?} => {:#?})", &barrier.resource.uid(), barrier.src_access, barrier.dst_access, barrier.src_stage, barrier.dst_stage))}
            Node::_Unreachable(_) => { unreachable!() }
        }
    }
}

