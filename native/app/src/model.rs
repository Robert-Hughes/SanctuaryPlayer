use std::time::Duration;

use crate::video::VideoSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackState {
    Loading,
    Paused,
    Playing,
    Buffering,
    Seeking,
    Ended,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugInfoSection {
    pub title: String,
    pub rows: Vec<(String, String)>,
}

impl DebugInfoSection {
    pub fn new(title: impl Into<String>, rows: Vec<(String, String)>) -> Self {
        Self {
            title: title.into(),
            rows,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugGraphLane {
    Shared,
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugEdgeKind {
    Flow,
    Relationship,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugNode {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub lane: DebugGraphLane,
    pub column: u8,
    pub rows: Vec<(String, String)>,
}

impl DebugNode {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        summary: impl Into<String>,
        lane: DebugGraphLane,
        column: u8,
        rows: Vec<(String, String)>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            summary: summary.into(),
            lane,
            column,
            rows,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugEdge {
    pub from: String,
    pub to: String,
    pub label: String,
    pub kind: DebugEdgeKind,
}

impl DebugEdge {
    pub fn flow(from: impl Into<String>, to: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            label: label.into(),
            kind: DebugEdgeKind::Flow,
        }
    }

    pub fn relationship(
        from: impl Into<String>,
        to: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            label: label.into(),
            kind: DebugEdgeKind::Relationship,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugGraph {
    pub nodes: Vec<DebugNode>,
    pub edges: Vec<DebugEdge>,
}

impl DebugGraph {
    pub fn extend(&mut self, other: Self) {
        self.nodes.extend(other.nodes);
        self.edges.extend(other.edges);
    }

    pub fn retain_resolved_edges(&mut self) {
        let node_ids = self
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<std::collections::HashSet<_>>();
        self.edges
            .retain(|edge| node_ids.contains(&edge.from) && node_ids.contains(&edge.to));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quality {
    pub id: String,
    pub label: String,
}

impl Quality {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppCommand {
    OpenVideo(VideoSource),
    TogglePlayback,
    RefreshPlayback,
    ToggleDebugInfo,
    Play,
    Pause,
    SeekAbsolute(Duration),
    SeekRelative(i64),
    SetPlaybackRate(f32),
    SetVolume(f32),
    SetQuality(String),
    SetFavouriteQualities(String),
    SignIn { user_id: String, device_id: String },
    SignOut,
    ToggleFullscreen,
    ToggleControlsLock,
    ToggleControlsVisibility,
}

#[cfg(test)]
mod debug_graph_tests {
    use super::*;

    #[test]
    fn graph_extend_preserves_stable_nodes_lanes_and_edge_kinds() {
        let mut graph = DebugGraph {
            nodes: vec![DebugNode::new(
                "source",
                "Source",
                "HLS",
                DebugGraphLane::Shared,
                0,
                vec![],
            )],
            edges: vec![],
        };
        graph.extend(DebugGraph {
            nodes: vec![DebugNode::new(
                "decoder",
                "Decoder",
                "h264_vulkan",
                DebugGraphLane::Video,
                2,
                vec![("acceleration".into(), "hardware".into())],
            )],
            edges: vec![
                DebugEdge::flow("source", "decoder", "compressed packets"),
                DebugEdge::relationship("decoder", "source", "shared runtime"),
            ],
        });

        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["source", "decoder"]
        );
        assert_eq!(graph.nodes[1].lane, DebugGraphLane::Video);
        assert_eq!(graph.nodes[1].column, 2);
        assert_eq!(graph.edges[0].kind, DebugEdgeKind::Flow);
        assert_eq!(graph.edges[1].kind, DebugEdgeKind::Relationship);
    }

    #[test]
    fn retain_resolved_edges_removes_only_dangling_relationships() {
        let mut graph = DebugGraph {
            nodes: vec![
                DebugNode::new("source", "Source", "", DebugGraphLane::Shared, 0, vec![]),
                DebugNode::new("decoder", "Decoder", "", DebugGraphLane::Video, 1, vec![]),
            ],
            edges: vec![
                DebugEdge::flow("source", "decoder", "packets"),
                DebugEdge::relationship("decoder", "missing-device", "same device"),
                DebugEdge::relationship("missing-source", "decoder", "unknown"),
            ],
        };

        graph.retain_resolved_edges();

        assert_eq!(graph.edges.len(), 1);
        assert_eq!(
            graph.edges[0],
            DebugEdge::flow("source", "decoder", "packets")
        );
    }
}
