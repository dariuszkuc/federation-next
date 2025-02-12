use crate::error::{FederationError, SingleFederationError};
use crate::query_plan::operation::normalized_field_selection::NormalizedField;
use crate::query_plan::operation::normalized_inline_fragment_selection::NormalizedInlineFragment;
use crate::query_plan::operation::NormalizedSelectionSet;
use crate::schema::position::{
    CompositeTypeDefinitionPosition, FieldDefinitionPosition, InterfaceFieldDefinitionPosition,
    ObjectTypeDefinitionPosition, OutputTypeDefinitionPosition, SchemaRootDefinitionKind,
};
use crate::schema::ValidFederationSchema;
use apollo_compiler::schema::{Name, NamedType};
use apollo_compiler::NodeStr;
use indexmap::{IndexMap, IndexSet};
use petgraph::graph::{DiGraph, EdgeIndex, EdgeReference, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use std::fmt::{Display, Formatter};
use std::hash::Hash;
use std::sync::Arc;

pub mod build_query_graph;
pub(crate) mod condition_resolver;
pub(crate) mod extract_subgraphs_from_supergraph;
mod field_set;
pub(crate) mod graph_path;
pub(crate) mod path_tree;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct QueryGraphNode {
    /// The GraphQL type this node points to.
    pub(crate) type_: QueryGraphNodeType,
    /// An identifier of the underlying schema containing the `type_` this node points to. This is
    /// mainly used in federated query graphs, where the `source` is a subgraph name.
    pub(crate) source: NodeStr,
    /// True if there is a cross-subgraph edge that is reachable from this node.
    pub(crate) has_reachable_cross_subgraph_edges: bool,
    /// @provides works by creating duplicates of the node/type involved in the provides and adding
    /// the provided edges only to those copies. This means that with @provides, you can have more
    /// than one node per-type-and-subgraph in a query graph. Which is fine, but this `provide_id`
    /// allows distinguishing if a node was created as part of this @provides duplication or not.
    /// The value of this field has no other meaning than to be unique per-@provide, and so all the
    /// nodes copied for a given @provides application will have the same `provide_id`. Overall,
    /// this mostly exists for debugging visualization.
    pub(crate) provide_id: Option<u32>,
    // If present, this node represents a root node of the corresponding kind.
    pub(crate) root_kind: Option<SchemaRootDefinitionKind>,
}

impl Display for QueryGraphNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.type_, self.source)?;
        if let Some(provide_id) = self.provide_id {
            write!(f, "-{}", provide_id)?;
        }
        if self.root_kind.is_some() {
            write!(f, "*")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From)]
pub(crate) enum QueryGraphNodeType {
    SchemaType(OutputTypeDefinitionPosition),
    FederatedRootType(SchemaRootDefinitionKind),
}

impl Display for QueryGraphNodeType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryGraphNodeType::SchemaType(pos) => pos.fmt(f),
            QueryGraphNodeType::FederatedRootType(root_kind) => {
                write!(f, "[{}]", root_kind)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct QueryGraphEdge {
    /// Indicates what kind of edge this is and what the edge does/represents. For instance, if the
    /// edge represents a field, the `transition` will be a `FieldCollection` transition and will
    /// link to the definition of the field it represents.
    pub(crate) transition: QueryGraphEdgeTransition,
    /// Optional conditions on an edge.
    ///
    /// Conditions are a select of selections (in the GraphQL sense) that the traversal of a query
    /// graph needs to "collect" (traverse edges with transitions corresponding to those selections)
    /// in order to be able to collect that edge.
    ///
    /// Conditions are primarily used for edges corresponding to @key, in which case they correspond
    /// to the fields composing the @key. In other words, for an @key edge, conditions basically
    /// represent the fact that you need the key to be able to use an @key edge.
    ///
    /// Outside of keys, @requires edges also rely on conditions.
    pub(crate) conditions: Option<Arc<NormalizedSelectionSet>>,
}

impl Display for QueryGraphEdge {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if matches!(
            self.transition,
            QueryGraphEdgeTransition::SubgraphEnteringTransition
        ) && self.conditions.is_none()
        {
            return Ok(());
        }
        if let Some(conditions) = &self.conditions {
            write!(f, "{} ⊢ {}", conditions, self.transition)
        } else {
            self.transition.fmt(f)
        }
    }
}

/// The type of query graph edge "transition".
///
/// An edge transition encodes what the edge corresponds to, in the underlying GraphQL schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum QueryGraphEdgeTransition {
    /// A field edge, going from (a node for) the field parent type to the field's (base) type.
    FieldCollection {
        /// The name of the schema containing the field.
        source: NodeStr,
        /// The object/interface field being collected.
        field_definition_position: FieldDefinitionPosition,
        /// Whether this field is part of an @provides.
        is_part_of_provides: bool,
    },
    /// A downcast edge, going from a composite type (object, interface, or union) to another
    /// composite type that intersects that type (i.e. has at least one possible runtime object type
    /// in common with it).
    Downcast {
        /// The name of the schema containing the from/to types.
        source: NodeStr,
        /// The parent type of the type condition, i.e. the type of the selection set containing
        /// the type condition.
        from_type_position: CompositeTypeDefinitionPosition,
        /// The type of the type condition, i.e. the type coming after "... on".
        to_type_position: CompositeTypeDefinitionPosition,
    },
    /// A key edge (only found in federated query graphs) going from an entity type in a particular
    /// subgraph to the same entity type but in another subgraph. Key transition edges _must_ have
    /// `conditions` corresponding to the key fields.
    KeyResolution,
    /// A root type edge (only found in federated query graphs) going from a root type (query,
    /// mutation or subscription) of a subgraph to the (same) root type of another subgraph. It
    /// encodes the fact that if a subgraph field returns a root type, any subgraph can be queried
    /// from there.
    RootTypeResolution {
        /// The kind of schema root resolved.
        root_kind: SchemaRootDefinitionKind,
    },
    /// A subgraph-entering edge, which is a special case only used for edges coming out of the root
    /// nodes of "federated" query graphs. It does not correspond to any physical GraphQL elements
    /// but can be understood as the fact that the router is always free to start querying any of
    /// the subgraph services as needed.
    SubgraphEnteringTransition,
    /// A "fake" downcast edge (only found in federated query graphs) going from an @interfaceObject
    /// type to an implementation. This encodes the fact that an @interfaceObject type "stands-in"
    /// for any possible implementations (in the supergraph) of the corresponding interface. It is
    /// "fake" because the corresponding edge stays on the @interfaceObject type (this is also why
    /// the "to type" is only a name: that to/casted type does not actually exist in the subgraph
    /// in which the corresponding edge will be found).
    InterfaceObjectFakeDownCast {
        /// The name of the schema containing the from type.
        source: NodeStr,
        /// The parent type of the type condition, i.e. the type of the selection set containing
        /// the type condition.
        from_type_position: CompositeTypeDefinitionPosition,
        /// The type of the type condition, i.e. the type coming after "... on".
        to_type_name: Name,
    },
}

impl QueryGraphEdgeTransition {
    pub(crate) fn collect_operation_elements(&self) -> bool {
        match self {
            QueryGraphEdgeTransition::FieldCollection { .. } => true,
            QueryGraphEdgeTransition::Downcast { .. } => true,
            QueryGraphEdgeTransition::KeyResolution => false,
            QueryGraphEdgeTransition::RootTypeResolution { .. } => false,
            QueryGraphEdgeTransition::SubgraphEnteringTransition => false,
            QueryGraphEdgeTransition::InterfaceObjectFakeDownCast { .. } => true,
        }
    }
}

impl Display for QueryGraphEdgeTransition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryGraphEdgeTransition::FieldCollection {
                field_definition_position,
                ..
            } => {
                write!(f, "{}", field_definition_position.field_name())
            }
            QueryGraphEdgeTransition::Downcast {
                to_type_position, ..
            } => {
                write!(f, "... on {}", to_type_position.type_name())
            }
            QueryGraphEdgeTransition::KeyResolution => {
                write!(f, "key()")
            }
            QueryGraphEdgeTransition::RootTypeResolution { root_kind } => {
                write!(f, "{}()", root_kind)
            }
            QueryGraphEdgeTransition::SubgraphEnteringTransition => {
                write!(f, "∅")
            }
            QueryGraphEdgeTransition::InterfaceObjectFakeDownCast { to_type_name, .. } => {
                write!(f, "... on {}", to_type_name)
            }
        }
    }
}

#[derive(Debug)]
pub struct QueryGraph {
    /// The "current" source of the query graph. For query graphs representing a single source
    /// graph, this will only ever be one value, but it will change for "federated" query graphs
    /// while they're being built (and after construction, will become FEDERATED_GRAPH_ROOT_SOURCE,
    /// which is a reserved placeholder value).
    current_source: NodeStr,
    /// The nodes/edges of the query graph. Note that nodes/edges should never be removed, so
    /// indexes are immutable when a node/edge is created.
    graph: DiGraph<QueryGraphNode, QueryGraphEdge>,
    /// The sources on which the query graph was built, which is a set (potentially of size 1) of
    /// GraphQL schema keyed by the name identifying them. Note that the `source` strings in the
    /// nodes/edges of a query graph are guaranteed to be valid key in this map.
    sources: IndexMap<NodeStr, ValidFederationSchema>,
    /// A map (keyed by source) that associates type names of the underlying schema on which this
    /// query graph was built to each of the nodes that points to a type of that name. Note that for
    /// a "federated" query graph source, each type name will only map to a single node.
    types_to_nodes_by_source: IndexMap<NodeStr, IndexMap<NamedType, IndexSet<NodeIndex>>>,
    /// A map (keyed by source) that associates schema root kinds to root nodes.
    root_kinds_to_nodes_by_source: IndexMap<NodeStr, IndexMap<SchemaRootDefinitionKind, NodeIndex>>,
    /// Maps an edge to the possible edges that can follow it "productively", that is without
    /// creating a trivially inefficient path.
    ///
    /// More precisely, this map is equivalent to looking at the out edges of a given edge's tail
    /// node and filtering those edges that "never make sense" after the given edge, which mainly
    /// amounts to avoiding chaining @key edges when we know there is guaranteed to be a better
    /// option. As an example, suppose we have 3 subgraphs A, B and C which all defined an
    /// `@key(fields: "id")` on some entity type `T`. Then it is never interesting to take that @key
    /// edge from B -> C after A -> B because if we're in A and want to get to C, we can always do
    /// A -> C (of course, this is only true because it's the "same" key).
    ///
    /// See `precompute_non_trivial_followup_edges` for more details on which exact edges are
    /// filtered.
    ///
    /// Lastly, note that the main reason for having this field is that its result is pre-computed.
    /// Which in turn is done for performance reasons: having the same key defined in multiple
    /// subgraphs is _the_ most common pattern, and while our later algorithms (composition
    /// validation and query planning) would know to not select those trivially inefficient
    /// "detours", they might have to redo those checks many times and pre-computing once it is
    /// significantly faster (and pretty easy). FWIW, when originally introduced, this optimization
    /// lowered composition validation on a big composition (100+ subgraphs) from ~4 minutes to
    /// ~10 seconds.
    non_trivial_followup_edges: IndexMap<EdgeIndex, IndexSet<EdgeIndex>>,
}

impl QueryGraph {
    pub(crate) fn name(&self) -> &str {
        &self.current_source
    }

    pub(crate) fn graph(&self) -> &DiGraph<QueryGraphNode, QueryGraphEdge> {
        &self.graph
    }

    pub(crate) fn node_weight(&self, node: NodeIndex) -> Result<&QueryGraphNode, FederationError> {
        self.graph.node_weight(node).ok_or_else(|| {
            SingleFederationError::Internal {
                message: "Node unexpectedly missing".to_owned(),
            }
            .into()
        })
    }

    fn node_weight_mut(&mut self, node: NodeIndex) -> Result<&mut QueryGraphNode, FederationError> {
        self.graph.node_weight_mut(node).ok_or_else(|| {
            SingleFederationError::Internal {
                message: "Node unexpectedly missing".to_owned(),
            }
            .into()
        })
    }

    pub(crate) fn edge_weight(&self, edge: EdgeIndex) -> Result<&QueryGraphEdge, FederationError> {
        self.graph.edge_weight(edge).ok_or_else(|| {
            SingleFederationError::Internal {
                message: "Edge unexpectedly missing".to_owned(),
            }
            .into()
        })
    }

    fn edge_weight_mut(&mut self, edge: EdgeIndex) -> Result<&mut QueryGraphEdge, FederationError> {
        self.graph.edge_weight_mut(edge).ok_or_else(|| {
            SingleFederationError::Internal {
                message: "Edge unexpectedly missing".to_owned(),
            }
            .into()
        })
    }

    pub(crate) fn edge_endpoints(
        &self,
        edge: EdgeIndex,
    ) -> Result<(NodeIndex, NodeIndex), FederationError> {
        self.graph.edge_endpoints(edge).ok_or_else(|| {
            SingleFederationError::Internal {
                message: "Edge unexpectedly missing".to_owned(),
            }
            .into()
        })
    }

    pub(crate) fn schema(&self) -> Result<&ValidFederationSchema, FederationError> {
        self.schema_by_source(&self.current_source)
    }

    pub(crate) fn schema_by_source(
        &self,
        source: &str,
    ) -> Result<&ValidFederationSchema, FederationError> {
        self.sources.get(source).ok_or_else(|| {
            SingleFederationError::Internal {
                message: "Schema unexpectedly missing".to_owned(),
            }
            .into()
        })
    }

    pub(crate) fn types_to_nodes(
        &self,
    ) -> Result<&IndexMap<NamedType, IndexSet<NodeIndex>>, FederationError> {
        self.types_to_nodes_by_source(&self.current_source)
    }

    fn types_to_nodes_by_source(
        &self,
        source: &str,
    ) -> Result<&IndexMap<NamedType, IndexSet<NodeIndex>>, FederationError> {
        self.types_to_nodes_by_source.get(source).ok_or_else(|| {
            SingleFederationError::Internal {
                message: "Types-to-nodes map unexpectedly missing".to_owned(),
            }
            .into()
        })
    }

    fn types_to_nodes_mut(
        &mut self,
    ) -> Result<&mut IndexMap<NamedType, IndexSet<NodeIndex>>, FederationError> {
        self.types_to_nodes_by_source
            .get_mut(&self.current_source)
            .ok_or_else(|| {
                SingleFederationError::Internal {
                    message: "Types-to-nodes map unexpectedly missing".to_owned(),
                }
                .into()
            })
    }

    pub(crate) fn root_kinds_to_nodes(
        &self,
    ) -> Result<&IndexMap<SchemaRootDefinitionKind, NodeIndex>, FederationError> {
        self.root_kinds_to_nodes_by_source
            .get(&self.current_source)
            .ok_or_else(|| {
                SingleFederationError::Internal {
                    message: "Root-kinds-to-nodes map unexpectedly missing".to_owned(),
                }
                .into()
            })
    }

    fn root_kinds_to_nodes_mut(
        &mut self,
    ) -> Result<&mut IndexMap<SchemaRootDefinitionKind, NodeIndex>, FederationError> {
        self.root_kinds_to_nodes_by_source
            .get_mut(&self.current_source)
            .ok_or_else(|| {
                SingleFederationError::Internal {
                    message: "Root-kinds-to-nodes map unexpectedly missing".to_owned(),
                }
                .into()
            })
    }

    pub(crate) fn non_trivial_followup_edges(&self) -> &IndexMap<EdgeIndex, IndexSet<EdgeIndex>> {
        &self.non_trivial_followup_edges
    }

    /// All outward edges from the given node (including self-key and self-root-type-resolution
    /// edges). Primarily used by `@defer`, when needing to re-enter a subgraph for a deferred
    /// section.
    pub(crate) fn out_edges_with_federation_self_edges(
        &self,
        node: NodeIndex,
    ) -> impl Iterator<Item = EdgeReference<QueryGraphEdge>> {
        self.graph.edges_directed(node, Direction::Outgoing)
    }

    /// The outward edges from the given node, minus self-key and self-root-type-resolution edges,
    /// as they're rarely useful (currently only used by `@defer`).
    pub(crate) fn out_edges(
        &self,
        node: NodeIndex,
    ) -> impl Iterator<Item = EdgeReference<QueryGraphEdge>> {
        self.graph
            .edges_directed(node, Direction::Outgoing)
            .filter(|edge_ref| {
                !(edge_ref.source() == edge_ref.target()
                    && matches!(
                        edge_ref.weight().transition,
                        QueryGraphEdgeTransition::KeyResolution
                            | QueryGraphEdgeTransition::RootTypeResolution { .. }
                    ))
            })
    }

    pub(crate) fn edge_for_field(
        &self,
        node: NodeIndex,
        field: &NormalizedField,
    ) -> Option<EdgeIndex> {
        let mut candidates = self.out_edges(node).filter_map(|edge_ref| {
            let edge_weight = edge_ref.weight();
            let QueryGraphEdgeTransition::FieldCollection {
                field_definition_position,
                ..
            } = &edge_weight.transition
            else {
                return None;
            };
            // We explicitly avoid comparing parent type's here, to allow interface object
            // fields to match operation fields with the same name but differing types.
            if field.data().field_position.field_name() == field_definition_position.field_name() {
                Some(edge_ref.id())
            } else {
                None
            }
        });
        if let Some(candidate) = candidates.next() {
            // PORT_NOTE: The JS codebase used an assertion rather than a debug assertion here. We
            // consider it unlikely for there to be more than one candidate given all the code paths
            // that create edges, so we've downgraded this to a debug assertion.
            debug_assert!(
                candidates.next().is_none(),
                "Unexpectedly found multiple candidates",
            );
            Some(candidate)
        } else {
            None
        }
    }

    pub(crate) fn edge_for_inline_fragment(
        &self,
        node: NodeIndex,
        inline_fragment: &NormalizedInlineFragment,
    ) -> Option<EdgeIndex> {
        let Some(type_condition_pos) = &inline_fragment.data().type_condition_position else {
            // No type condition means the type hasn't changed, meaning there is no edge to take.
            return None;
        };
        let mut candidates = self.out_edges(node).filter_map(|edge_ref| {
            let edge_weight = edge_ref.weight();
            let QueryGraphEdgeTransition::Downcast {
                to_type_position, ..
            } = &edge_weight.transition
            else {
                return None;
            };
            // We explicitly avoid comparing type kinds, to allow interface object types to
            // match operation inline fragments (where the supergraph type kind is interface,
            // but the subgraph type kind is object).
            if type_condition_pos.type_name() == to_type_position.type_name() {
                Some(edge_ref.id())
            } else {
                None
            }
        });
        if let Some(candidate) = candidates.next() {
            // PORT_NOTE: The JS codebase used an assertion rather than a debug assertion here. We
            // consider it unlikely for there to be more than one candidate given all the code paths
            // that create edges, so we've downgraded this to a debug assertion.
            debug_assert!(
                candidates.next().is_none(),
                "Unexpectedly found multiple candidates",
            );
            Some(candidate)
        } else {
            None
        }
    }

    /// Given the possible runtime types at the head of the given edge, returns the possible runtime
    /// types after traversing the edge.
    // PORT_NOTE: Named `updateRuntimeTypes` in the JS codebase.
    pub(crate) fn advance_possible_runtime_types(
        &self,
        possible_runtime_types: &IndexSet<ObjectTypeDefinitionPosition>,
        edge: Option<EdgeIndex>,
    ) -> Result<IndexSet<ObjectTypeDefinitionPosition>, FederationError> {
        let Some(edge) = edge else {
            return Ok(possible_runtime_types.clone());
        };

        let edge_weight = self.edge_weight(edge)?;
        let (_, tail) = self.edge_endpoints(edge)?;
        let tail_weight = self.node_weight(tail)?;
        let QueryGraphNodeType::SchemaType(tail_type_pos) = &tail_weight.type_ else {
            return Err(FederationError::internal(
                "Unexpectedly encountered federation root node as tail node.",
            ));
        };
        return match &edge_weight.transition {
            QueryGraphEdgeTransition::FieldCollection {
                source,
                field_definition_position,
                ..
            } => {
                let Ok(_): Result<CompositeTypeDefinitionPosition, _> =
                    tail_type_pos.clone().try_into()
                else {
                    return Ok(IndexSet::new());
                };
                let schema = self.schema_by_source(source)?;
                let mut new_possible_runtime_types = IndexSet::new();
                for possible_runtime_type in possible_runtime_types {
                    let field_pos =
                        possible_runtime_type.field(field_definition_position.field_name().clone());
                    let Some(field) = field_pos.try_get(schema.schema()) else {
                        continue;
                    };
                    let field_type_pos: CompositeTypeDefinitionPosition = schema
                        .get_type(field.ty.inner_named_type().clone())?
                        .try_into()?;
                    new_possible_runtime_types
                        .extend(schema.possible_runtime_types(field_type_pos)?);
                }
                Ok(new_possible_runtime_types)
            }
            QueryGraphEdgeTransition::Downcast {
                source,
                to_type_position,
                ..
            } => Ok(self
                .schema_by_source(source)?
                .possible_runtime_types(to_type_position.clone())?
                .intersection(possible_runtime_types)
                .cloned()
                .collect()),
            QueryGraphEdgeTransition::KeyResolution => {
                let tail_type_pos: CompositeTypeDefinitionPosition =
                    tail_type_pos.clone().try_into()?;
                Ok(self
                    .schema_by_source(&tail_weight.source)?
                    .possible_runtime_types(tail_type_pos)?)
            }
            QueryGraphEdgeTransition::RootTypeResolution { .. } => {
                let OutputTypeDefinitionPosition::Object(tail_type_pos) = tail_type_pos.clone()
                else {
                    return Err(FederationError::internal(
                        "Unexpectedly encountered non-object root operation type.",
                    ));
                };
                Ok(IndexSet::from([tail_type_pos]))
            }
            QueryGraphEdgeTransition::SubgraphEnteringTransition => {
                let OutputTypeDefinitionPosition::Object(tail_type_pos) = tail_type_pos.clone()
                else {
                    return Err(FederationError::internal(
                        "Unexpectedly encountered non-object root operation type.",
                    ));
                };
                Ok(IndexSet::from([tail_type_pos]))
            }
            QueryGraphEdgeTransition::InterfaceObjectFakeDownCast { .. } => {
                Ok(possible_runtime_types.clone())
            }
        };
    }

    pub(crate) fn get_locally_satisfiable_key(
        &self,
        _node: NodeIndex,
    ) -> Result<Option<NormalizedSelectionSet>, FederationError> {
        todo!()
    }

    pub(crate) fn is_cross_subgraph_edge(&self, edge: EdgeIndex) -> Result<bool, FederationError> {
        let (head, tail) = self.edge_endpoints(edge)?;
        let head_weight = self.node_weight(head)?;
        let tail_weight = self.node_weight(tail)?;
        Ok(head_weight.source != tail_weight.source)
    }

    pub(crate) fn is_provides_edge(&self, edge: EdgeIndex) -> Result<bool, FederationError> {
        let edge_weight = self.edge_weight(edge)?;
        let QueryGraphEdgeTransition::FieldCollection {
            is_part_of_provides,
            ..
        } = &edge_weight.transition
        else {
            return Ok(false);
        };
        Ok(*is_part_of_provides)
    }

    pub(crate) fn has_an_implementation_with_provides(
        &self,
        _source: &NodeStr,
        _interface_field_definition_position: InterfaceFieldDefinitionPosition,
    ) -> Result<bool, FederationError> {
        todo!()
    }
}
