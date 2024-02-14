use crate::error::SingleFederationError::Internal;
use crate::error::{FederationError, MultipleFederationErrors};
use crate::link::federation_spec_definition::get_federation_spec_definition_from_subgraph;
use crate::query_graph::extract_subgraphs_from_supergraph::ValidFederationSubgraph;
use crate::query_graph::graph_path::OpPathElement;
use crate::query_plan::conditions::Conditions;
use crate::query_plan::operation::normalized_field_selection::{
    NormalizedField, NormalizedFieldData, NormalizedFieldSelection,
};
use crate::query_plan::operation::normalized_fragment_spread_selection::{
    NormalizedFragmentSpread, NormalizedFragmentSpreadData, NormalizedFragmentSpreadSelection,
};
use crate::query_plan::operation::normalized_inline_fragment_selection::{
    NormalizedInlineFragment, NormalizedInlineFragmentData, NormalizedInlineFragmentSelection,
};
use crate::query_plan::operation::normalized_selection_map::{
    Entry, NormalizedFieldSelectionValue, NormalizedFragmentSpreadSelectionValue,
    NormalizedInlineFragmentSelectionValue, NormalizedSelectionMap, NormalizedSelectionValue,
};
use crate::schema::position::{
    CompositeTypeDefinitionPosition, InterfaceTypeDefinitionPosition, ObjectTypeDefinitionPosition,
    SchemaRootDefinitionKind,
};
use crate::schema::ValidFederationSchema;
use apollo_compiler::ast::{DirectiveList, Name, OperationType};
use apollo_compiler::executable::{
    Field, Fragment, FragmentSpread, InlineFragment, Operation, Selection, SelectionSet,
    VariableDefinition,
};
use apollo_compiler::{name, Node};
use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::iter::Map;
use std::ops::Deref;
use std::sync::{atomic, Arc};

const TYPENAME_FIELD: Name = name!("__typename");

// Global storage for the counter used to uniquely identify selections
static NEXT_ID: atomic::AtomicUsize = atomic::AtomicUsize::new(1);

// opaque wrapper of the unique selection ID type
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct SelectionId(usize);

impl SelectionId {
    fn new() -> Self {
        // atomically increment global counter
        Self(NEXT_ID.fetch_add(1, atomic::Ordering::AcqRel))
    }
}

/// An analogue of the apollo-compiler type `Operation` with these changes:
/// - Stores the schema that the operation is queried against.
/// - Swaps `operation_type` with `root_kind` (using the analogous federation-next type).
/// - Encloses collection types in `Arc`s to facilitate cheaper cloning.
/// - Stores the fragments used by this operation (the executable document the operation was taken
///   from may contain other fragments that are not used by this operation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedOperation {
    pub(crate) schema: ValidFederationSchema,
    pub(crate) root_kind: SchemaRootDefinitionKind,
    pub(crate) name: Option<Name>,
    pub(crate) variables: Arc<Vec<Node<VariableDefinition>>>,
    pub(crate) directives: Arc<DirectiveList>,
    pub(crate) selection_set: NormalizedSelectionSet,
    pub(crate) fragments: Arc<HashMap<Name, Node<NormalizedFragment>>>,
}

/// An analogue of the apollo-compiler type `SelectionSet` with these changes:
/// - For the type, stores the schema and the position in that schema instead of just the
///   `NamedType`.
/// - Stores selections in a map so they can be normalized efficiently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NormalizedSelectionSet {
    pub(crate) schema: ValidFederationSchema,
    pub(crate) type_position: CompositeTypeDefinitionPosition,
    pub(crate) selections: Arc<NormalizedSelectionMap>,
}

pub(crate) mod normalized_selection_map {
    use crate::error::FederationError;
    use crate::error::SingleFederationError::Internal;
    use crate::query_plan::operation::normalized_field_selection::NormalizedFieldSelection;
    use crate::query_plan::operation::normalized_fragment_spread_selection::NormalizedFragmentSpreadSelection;
    use crate::query_plan::operation::normalized_inline_fragment_selection::NormalizedInlineFragmentSelection;
    use crate::query_plan::operation::{
        HasNormalizedSelectionKey, NormalizedSelection, NormalizedSelectionKey,
        NormalizedSelectionSet,
    };
    use apollo_compiler::ast::Name;
    use indexmap::IndexMap;
    use std::borrow::Borrow;
    use std::hash::Hash;
    use std::iter::Map;
    use std::ops::Deref;
    use std::sync::Arc;

    /// A "normalized" selection map is an optimized representation of a selection set which does
    /// not contain selections with the same selection "key". Selections that do have the same key
    /// are  merged during the normalization process. By storing a selection set as a map, we can
    /// efficiently merge/join multiple selection sets.
    ///
    /// Because the key depends strictly on the value, we expose the underlying map's API in a
    /// read-only capacity, while mutations use an API closer to `IndexSet`. We don't just use an
    /// `IndexSet` since key computation is expensive (it involves sorting). This type is in its own
    /// module to prevent code from accidentally mutating the underlying map outside the mutation
    /// API.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub(crate) struct NormalizedSelectionMap(IndexMap<NormalizedSelectionKey, NormalizedSelection>);

    impl Deref for NormalizedSelectionMap {
        type Target = IndexMap<NormalizedSelectionKey, NormalizedSelection>;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl NormalizedSelectionMap {
        pub(crate) fn new() -> Self {
            NormalizedSelectionMap(IndexMap::new())
        }

        pub(crate) fn clear(&mut self) {
            self.0.clear();
        }

        pub(crate) fn insert(&mut self, value: NormalizedSelection) -> Option<NormalizedSelection> {
            self.0.insert(value.key(), value)
        }

        pub(crate) fn remove<Q: ?Sized>(&mut self, key: &Q) -> Option<NormalizedSelection>
        where
            NormalizedSelectionKey: Borrow<Q>,
            Q: Eq + Hash,
        {
            // We specifically use shift_remove() instead of swap_remove() to maintain order.
            self.0.shift_remove(key)
        }

        pub(crate) fn get_mut<Q: ?Sized>(&mut self, key: &Q) -> Option<NormalizedSelectionValue>
        where
            NormalizedSelectionKey: Borrow<Q>,
            Q: Eq + Hash,
        {
            self.0.get_mut(key).map(NormalizedSelectionValue::new)
        }

        pub(crate) fn iter_mut(&mut self) -> IterMut {
            self.0
                .iter_mut()
                .map(|(k, v)| (k, NormalizedSelectionValue::new(v)))
        }

        pub(super) fn entry(&mut self, key: NormalizedSelectionKey) -> Entry {
            match self.0.entry(key) {
                indexmap::map::Entry::Occupied(entry) => Entry::Occupied(OccupiedEntry(entry)),
                indexmap::map::Entry::Vacant(entry) => Entry::Vacant(VacantEntry(entry)),
            }
        }
    }

    type IterMut<'a> = Map<
        indexmap::map::IterMut<'a, NormalizedSelectionKey, NormalizedSelection>,
        fn(
            (&'a NormalizedSelectionKey, &'a mut NormalizedSelection),
        ) -> (&'a NormalizedSelectionKey, NormalizedSelectionValue<'a>),
    >;

    /// A mutable reference to a `NormalizedSelection` value in a `NormalizedSelectionMap`, which
    /// also disallows changing key-related data (to maintain the invariant that a value's key is
    /// the same as it's map entry's key).
    #[derive(Debug)]
    pub(crate) enum NormalizedSelectionValue<'a> {
        Field(NormalizedFieldSelectionValue<'a>),
        FragmentSpread(NormalizedFragmentSpreadSelectionValue<'a>),
        InlineFragment(NormalizedInlineFragmentSelectionValue<'a>),
    }

    impl<'a> NormalizedSelectionValue<'a> {
        pub(crate) fn new(selection: &'a mut NormalizedSelection) -> Self {
            match selection {
                NormalizedSelection::Field(field_selection) => NormalizedSelectionValue::Field(
                    NormalizedFieldSelectionValue::new(field_selection),
                ),
                NormalizedSelection::FragmentSpread(fragment_spread_selection) => {
                    NormalizedSelectionValue::FragmentSpread(
                        NormalizedFragmentSpreadSelectionValue::new(fragment_spread_selection),
                    )
                }
                NormalizedSelection::InlineFragment(inline_fragment_selection) => {
                    NormalizedSelectionValue::InlineFragment(
                        NormalizedInlineFragmentSelectionValue::new(inline_fragment_selection),
                    )
                }
            }
        }
    }

    #[derive(Debug)]
    pub(crate) struct NormalizedFieldSelectionValue<'a>(&'a mut Arc<NormalizedFieldSelection>);

    impl<'a> NormalizedFieldSelectionValue<'a> {
        pub(crate) fn new(field_selection: &'a mut Arc<NormalizedFieldSelection>) -> Self {
            Self(field_selection)
        }

        pub(crate) fn get(&self) -> &Arc<NormalizedFieldSelection> {
            self.0
        }

        pub(crate) fn get_selection_set_mut(&mut self) -> &mut Option<NormalizedSelectionSet> {
            &mut Arc::make_mut(self.0).selection_set
        }

        pub(crate) fn get_sibling_typename_mut(&mut self) -> &mut Option<Name> {
            &mut Arc::make_mut(self.0).sibling_typename
        }
    }

    #[derive(Debug)]
    pub(crate) struct NormalizedFragmentSpreadSelectionValue<'a>(
        &'a mut Arc<NormalizedFragmentSpreadSelection>,
    );

    impl<'a> NormalizedFragmentSpreadSelectionValue<'a> {
        pub(crate) fn new(
            fragment_spread_selection: &'a mut Arc<NormalizedFragmentSpreadSelection>,
        ) -> Self {
            Self(fragment_spread_selection)
        }

        pub(crate) fn get(&self) -> &Arc<NormalizedFragmentSpreadSelection> {
            self.0
        }
    }

    #[derive(Debug)]
    pub(crate) struct NormalizedInlineFragmentSelectionValue<'a>(
        &'a mut Arc<NormalizedInlineFragmentSelection>,
    );

    impl<'a> NormalizedInlineFragmentSelectionValue<'a> {
        pub(crate) fn new(
            inline_fragment_selection: &'a mut Arc<NormalizedInlineFragmentSelection>,
        ) -> Self {
            Self(inline_fragment_selection)
        }

        pub(crate) fn get(&self) -> &Arc<NormalizedInlineFragmentSelection> {
            self.0
        }

        pub(crate) fn get_selection_set_mut(&mut self) -> &mut NormalizedSelectionSet {
            &mut Arc::make_mut(self.0).selection_set
        }
    }

    pub(crate) enum Entry<'a> {
        Occupied(OccupiedEntry<'a>),
        Vacant(VacantEntry<'a>),
    }

    pub(crate) struct OccupiedEntry<'a>(
        indexmap::map::OccupiedEntry<'a, NormalizedSelectionKey, NormalizedSelection>,
    );

    impl<'a> OccupiedEntry<'a> {
        pub(crate) fn get(&self) -> &NormalizedSelection {
            self.0.get()
        }

        pub(crate) fn get_mut(&mut self) -> NormalizedSelectionValue {
            NormalizedSelectionValue::new(self.0.get_mut())
        }

        pub(crate) fn into_mut(self) -> NormalizedSelectionValue<'a> {
            NormalizedSelectionValue::new(self.0.into_mut())
        }

        pub(crate) fn key(&self) -> &NormalizedSelectionKey {
            self.0.key()
        }

        pub(crate) fn remove(self) -> NormalizedSelection {
            // We specifically use shift_remove() instead of swap_remove() to maintain order.
            self.0.shift_remove()
        }
    }

    pub(crate) struct VacantEntry<'a>(
        indexmap::map::VacantEntry<'a, NormalizedSelectionKey, NormalizedSelection>,
    );

    impl<'a> VacantEntry<'a> {
        pub(crate) fn key(&self) -> &NormalizedSelectionKey {
            self.0.key()
        }

        pub(crate) fn insert(
            self,
            value: NormalizedSelection,
        ) -> Result<NormalizedSelectionValue<'a>, FederationError> {
            if *self.key() != value.key() {
                return Err(Internal {
                    message: format!(
                        "Key mismatch when inserting selection {} into vacant entry ",
                        value
                    ),
                }
                .into());
            }
            Ok(NormalizedSelectionValue::new(self.0.insert(value)))
        }
    }

    impl IntoIterator for NormalizedSelectionMap {
        type Item = <IndexMap<NormalizedSelectionKey, NormalizedSelection> as IntoIterator>::Item;
        type IntoIter =
            <IndexMap<NormalizedSelectionKey, NormalizedSelection> as IntoIterator>::IntoIter;

        fn into_iter(self) -> Self::IntoIter {
            <IndexMap<NormalizedSelectionKey, NormalizedSelection> as IntoIterator>::into_iter(
                self.0,
            )
        }
    }
}

/// A selection "key" (unrelated to the federation `@key` directive) is an identifier of a selection
/// (field, inline fragment, or fragment spread) that is used to determine whether two selections
/// can be merged.
///
/// In order to merge two selections they need to
/// * reference the same field/inline fragment
/// * specify the same directives
/// * directives have to be applied in the same order
/// * directive arguments order does not matter (they get automatically sorted by their names).
/// * selection cannot specify @defer directive
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum NormalizedSelectionKey {
    Field {
        /// The field alias (if specified) or field name in the resulting selection set.
        response_name: Name,
        /// directives applied on the field
        directives: Arc<DirectiveList>,
    },
    FragmentSpread {
        /// The fragment name referenced in the spread.
        name: Name,
        /// Directives applied on the fragment spread (does not contain @defer).
        directives: Arc<DirectiveList>,
    },
    DeferredFragmentSpread {
        /// Unique selection ID used to distinguish deferred fragment spreads that cannot be merged.
        deferred_id: SelectionId,
    },
    InlineFragment {
        /// The optional type condition of the inline fragment.
        type_condition: Option<Name>,
        /// Directives applied on the inline fragment (does not contain @defer).
        directives: Arc<DirectiveList>,
    },
    DeferredInlineFragment {
        /// Unique selection ID used to distinguish deferred inline fragments that cannot be merged.
        deferred_id: SelectionId,
    },
}

pub(crate) trait HasNormalizedSelectionKey {
    fn key(&self) -> NormalizedSelectionKey;
}

/// An analogue of the apollo-compiler type `Selection` that stores our other selection analogues
/// instead of the apollo-compiler types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NormalizedSelection {
    Field(Arc<NormalizedFieldSelection>),
    FragmentSpread(Arc<NormalizedFragmentSpreadSelection>),
    InlineFragment(Arc<NormalizedInlineFragmentSelection>),
}

impl NormalizedSelection {
    fn directives(&self) -> &Arc<DirectiveList> {
        match self {
            NormalizedSelection::Field(field_selection) => &field_selection.field.data().directives,
            NormalizedSelection::FragmentSpread(fragment_spread_selection) => {
                &fragment_spread_selection.fragment_spread.data().directives
            }
            NormalizedSelection::InlineFragment(inline_fragment_selection) => {
                &inline_fragment_selection.inline_fragment.data().directives
            }
        }
    }

    pub(crate) fn element(&self) -> Result<OpPathElement, FederationError> {
        match self {
            NormalizedSelection::Field(field_selection) => {
                Ok(OpPathElement::Field(field_selection.field.clone()))
            }
            NormalizedSelection::FragmentSpread(_) => Err(Internal {
                message: "Fragment spread does not have element".to_owned(),
            }
            .into()),
            NormalizedSelection::InlineFragment(inline_fragment_selection) => Ok(
                OpPathElement::InlineFragment(inline_fragment_selection.inline_fragment.clone()),
            ),
        }
    }

    pub(crate) fn selection_set(&self) -> Result<Option<&NormalizedSelectionSet>, FederationError> {
        match self {
            NormalizedSelection::Field(field_selection) => {
                Ok(field_selection.selection_set.as_ref())
            }
            NormalizedSelection::FragmentSpread(_) => Err(Internal {
                message: "Fragment spread does not directly have a selection set".to_owned(),
            }
            .into()),
            NormalizedSelection::InlineFragment(inline_fragment_selection) => {
                Ok(Some(&inline_fragment_selection.selection_set))
            }
        }
    }

    pub(crate) fn conditions(&self) -> Result<Conditions, FederationError> {
        let self_conditions = Conditions::from_directives(self.directives())?;
        if let Conditions::Boolean(false) = self_conditions {
            // Never included, so there is no point recursing.
            Ok(Conditions::Boolean(false))
        } else {
            match self {
                NormalizedSelection::Field(_) => {
                    // The sub-selections of this field don't affect whether we should query this
                    // field, so we explicitly do not merge them in.
                    //
                    // PORT_NOTE: The JS codebase merges the sub-selections' conditions in with the
                    // field's conditions when field's selections are non-boolean. This is arguably
                    // a bug, so we've fixed it here.
                    Ok(self_conditions)
                }
                NormalizedSelection::InlineFragment(inline) => {
                    Ok(self_conditions.merge(inline.selection_set.conditions()?))
                }
                NormalizedSelection::FragmentSpread(_x) => Err(FederationError::internal(
                    "Unexpected fragment spread in NormalizedSelection::conditions()",
                )),
            }
        }
    }

    pub(crate) fn has_defer(&self) -> Result<bool, FederationError> {
        todo!()
    }

    fn collect_used_fragment_names(&self, aggregator: &mut Arc<HashMap<Name, i32>>) {
        match self {
            NormalizedSelection::Field(field_selection) => {
                if let Some(s) = field_selection.selection_set.clone() {
                    s.collect_used_fragment_names(aggregator)
                }
            }
            NormalizedSelection::InlineFragment(inline) => {
                inline.selection_set.collect_used_fragment_names(aggregator);
            }
            NormalizedSelection::FragmentSpread(fragment) => {
                let current_count = Arc::make_mut(aggregator)
                    .entry(fragment.fragment_spread.data().fragment_name.clone())
                    .or_default();
                *current_count += 1;
            }
        }
    }

    fn normalize(&self, parent_type: &CompositeTypeDefinitionPosition) -> NormalizedSelection {
        todo!()
    }
}

impl HasNormalizedSelectionKey for NormalizedSelection {
    fn key(&self) -> NormalizedSelectionKey {
        match self {
            NormalizedSelection::Field(field_selection) => field_selection.key(),
            NormalizedSelection::FragmentSpread(fragment_spread_selection) => {
                fragment_spread_selection.key()
            }
            NormalizedSelection::InlineFragment(inline_fragment_selection) => {
                inline_fragment_selection.key()
            }
        }
    }
}

/// An analogue of the apollo-compiler type `Fragment` with these changes:
/// - Stores the type condition explicitly, which means storing the schema and position (in
///   apollo-compiler, this is in the `SelectionSet`).
/// - Encloses collection types in `Arc`s to facilitate cheaper cloning.
#[derive(Debug, Clone, Eq)]
pub(crate) struct NormalizedFragment {
    pub(crate) schema: ValidFederationSchema,
    pub(crate) name: Name,
    pub(crate) type_condition_position: CompositeTypeDefinitionPosition,
    pub(crate) directives: Arc<DirectiveList>,
    pub(crate) selection_set: NormalizedSelectionSet,
    fragment_usages: Option<Arc<HashMap<Name, i32>>>,
    included_fragment_names: Option<Arc<HashSet<Name>>>,
}

impl PartialEq for NormalizedFragment {
    fn eq(&self, other: &Self) -> bool {
        self.schema == other.schema
            && self.name == other.name
            && self.type_condition_position == other.type_condition_position
            && self.directives == other.directives
            && self.selection_set == other.selection_set
    }
}

impl NormalizedFragment {
    fn new(
        schema: ValidFederationSchema,
        name: Name,
        type_condition_position: CompositeTypeDefinitionPosition,
        directives: Arc<DirectiveList>,
        selection_set: NormalizedSelectionSet,
    ) -> NormalizedFragment {
        NormalizedFragment {
            schema,
            name,
            type_condition_position,
            directives,
            selection_set,
            fragment_usages: None,
            included_fragment_names: None,
        }
    }

    pub(crate) fn normalize(
        fragment: &Fragment,
        schema: &ValidFederationSchema,
    ) -> Result<NormalizedFragment, FederationError> {
        Ok(Self {
            schema: schema.clone(),
            name: fragment.name.clone(),
            type_condition_position: schema
                .get_type(fragment.type_condition().clone())?
                .try_into()?,
            directives: Arc::new(fragment.directives.clone()),
            selection_set: NormalizedSelectionSet::normalize_and_expand_fragments(
                &fragment.selection_set,
                &IndexMap::new(),
                schema,
                &FragmentSpreadNormalizationOption::PreserveFragmentSpread,
            )?,
            fragment_usages: None,
            included_fragment_names: None,
        })
    }

    pub(crate) fn rebase_on(
        &self,
        schema: &ValidFederationSchema,
    ) -> Result<NormalizedFragment, FederationError> {
        // let fragment_parent = self.inline_fragment.data().parent_type_position.clone();
        // let type_condition = self.inline_fragment.data().type_condition_position.clone();
        //
        // if fragment_parent == *parent_type {
        //     return Ok(self.clone());
        // }
        //
        // let (can_rebase, condition) = self.can_rebase_on(parent_type, schema);
        // if !can_rebase {
        //     // Cannot add fragment of condition "${typeCondition}" (runtimes: [${possibleRuntimeTypes(typeCondition!)}]) to parent type "${parentType}" (runtimes: ${possibleRuntimeTypes(parentType)}
        //     return Err(FederationError::internal(
        //         format!("Cannot add fragment of condition {} (runtimes: []) to parent type {} (runtimes: [])",
        //                 type_condition.map_or_else(|| "undefined".to_string(), |c| c.type_name().to_string()),
        //                 parent_type.type_name()
        //         ),
        //
        //     ));
        // }
        //
        // let mut new_fragment_data = self.inline_fragment.data().clone();
        // new_fragment_data.type_condition_position = condition;
        todo!()
    }

    fn fragment_usages(&mut self) -> &Arc<HashMap<Name, i32>> {
        self.fragment_usages.get_or_insert_with(|| {
            let mut usages = Arc::new(HashMap::new());
            self.selection_set.collect_used_fragment_names(&mut usages);
            usages
        })
    }
}

pub(crate) enum RebaseErrorHandlingOption {
    IgnoreError,
    ThrowError,
}

pub(crate) struct RebasedFragments {
    original_fragments: NamedFragments,
    rebased_fragments: Arc<HashMap<String, Option<NamedFragments>>>,
}

impl RebasedFragments {
    pub(crate) fn new(fragments: &HashMap<Name, Node<NormalizedFragment>>) -> Self {
        Self {
            original_fragments: NamedFragments::new(Arc::new(fragments.clone())),
            rebased_fragments: Arc::new(HashMap::new()),
        }
    }

    pub(crate) fn for_subgraph(
        &mut self,
        subgraph: &ValidFederationSubgraph,
    ) -> Option<NamedFragments> {
        Arc::make_mut(&mut self.rebased_fragments)
            .entry(subgraph.name.clone())
            .or_insert_with(|| self.original_fragments.rebase_on(&subgraph.schema))
            .clone()
    }
}

#[derive(Clone)]
pub(crate) struct NamedFragments {
    fragments: Arc<HashMap<Name, Node<NormalizedFragment>>>,
}

impl NamedFragments {
    fn default() -> NamedFragments {
        NamedFragments {
            fragments: Arc::new(HashMap::new()),
        }
    }
    fn new(fragments: Arc<HashMap<Name, Node<NormalizedFragment>>>) -> NamedFragments {
        NamedFragments { fragments }
    }

    fn insert(&mut self, fragment: NormalizedFragment) {
        Arc::make_mut(&mut self.fragments).insert(fragment.name.clone(), Node::new(fragment));
    }

    fn contains(&self, name: &Name) -> bool {
        self.fragments.contains_key(name)
    }

    fn is_empty(&self) -> bool {
        self.fragments.len() == 0
    }

    fn get(&self, name: &Name) -> Option<Node<NormalizedFragment>> {
        self.fragments.get(name).cloned()
    }

    fn rebase_on(&mut self, schema: &ValidFederationSchema) -> Option<NamedFragments> {
        self.map_in_dependency_order(&|fragment, named_fragments| {
            if let Ok(rebased_type) = schema
                .get_type(fragment.type_condition_position.type_name().clone())
                .and_then(CompositeTypeDefinitionPosition::try_from)
            {
                if let Ok(mut rebased_selection) = fragment.selection_set.rebase_on(
                    &rebased_type,
                    named_fragments,
                    schema,
                    &RebaseErrorHandlingOption::IgnoreError,
                ) {
                    // Rebasing can leave some inefficiencies in some case (particularly when a spread has to be "expanded", see `FragmentSpreadSelection.rebaseOn`),
                    // so we do a top-level normalization to keep things clean.
                    rebased_selection = rebased_selection.normalize(&rebased_type);
                    if NamedFragments::is_selection_set_worth_using(&rebased_selection) {
                        NormalizedFragment::new(
                            schema.clone(),
                            fragment.name.clone(),
                            rebased_type.clone(),
                            fragment.directives.clone(),
                            rebased_selection,
                        );
                    }
                }
            }
            None
        })
    }

    /// The mapper is called on every fragment definition (`fragment` argument), but in such a way that if a fragment A uses another fragment B,
    /// then the mapper is guaranteed to be called on B _before_ being called on A. Further, the `newFragments` argument is a new `NamedFragments`
    /// containing all the previously mapped definition (minus those for which the mapper returned `undefined`). So if A uses B (and the mapper
    /// on B do not return undefined), then when mapper is called on A `newFragments` will have the mapped value for B.
    fn map_in_dependency_order(
        &mut self,
        mapper: &dyn Fn(&Node<NormalizedFragment>, &NamedFragments) -> Option<NormalizedFragment>,
    ) -> Option<NamedFragments> {
        struct FragmentDependencies {
            fragment: Node<NormalizedFragment>,
            depends_on: Vec<Name>,
        }
        let mut fragments_map: HashMap<Name, FragmentDependencies> = HashMap::new();
        Arc::make_mut(&mut self.fragments)
            .iter_mut()
            .for_each(|(_, fragment)| {
                let usages: Vec<Name> = fragment
                    .make_mut()
                    .fragment_usages()
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<Name>>();
                fragments_map.insert(
                    fragment.name.clone(),
                    FragmentDependencies {
                        fragment: fragment.clone(),
                        depends_on: usages,
                    },
                );
            });

        let mut removed_fragments: HashSet<Name> = HashSet::new();
        let mut mapped_fragments = NamedFragments::default();
        while !fragments_map.is_empty() {
            // Note that graphQL specifies that named fragments cannot have cycles (https://spec.graphql.org/draft/#sec-Fragment-spreads-must-not-form-cycles)
            // and so we're guaranteed that on every iteration, at least one element of the map is removed (so the `while` loop will terminate).
            fragments_map.retain(|name, info| {
                let can_remove = info
                    .depends_on
                    .iter()
                    .all(|n| mapped_fragments.contains(n) || removed_fragments.contains(n));
                if can_remove {
                    if let Some(mapped) = mapper(&info.fragment, &mapped_fragments) {
                        mapped_fragments.insert(mapped)
                    } else {
                        removed_fragments.insert(name.clone());
                    }
                }
                // keep only the elements that cannot be removed
                !can_remove
            });
        }

        if mapped_fragments.is_empty() {
            None
        } else {
            Some(mapped_fragments)
        }
    }

    /// When we rebase named fragments on a subgraph schema, only a subset of what the fragment handles may belong
    /// to that particular subgraph. And there are a few sub-cases where that subset is such that we basically need or
    /// want to consider to ignore the fragment for that subgraph, and that is when:
    /// 1. the subset that apply is actually empty. The fragment wouldn't be valid in this case anyway.
    /// 2. the subset is a single leaf field: in that case, using the one field directly is just shorter than using
    ///   the fragment, so we consider the fragment don't really apply to that subgraph. Technically, using the
    ///   fragment could still be of value if the fragment name is a lot smaller than the one field name, but it's
    ///   enough of a niche case that we ignore it. Note in particular that one sub-case of this rule that is likely
    ///   to be common is when the subset ends up being just `__typename`: this would basically mean the fragment
    ///   don't really apply to the subgraph, and that this will ensure this is the case.
    fn is_selection_set_worth_using(selection_set: &NormalizedSelectionSet) -> bool {
        if selection_set.selections.len() == 0 {
            return false;
        }
        if selection_set.selections.len() == 1 {
            // true if NOT field selection OR non-leaf field
            return if let Some((_, NormalizedSelection::Field(field_selection))) =
                selection_set.selections.first()
            {
                field_selection.selection_set.is_some()
            } else {
                true
            };
        }
        true
    }
}

pub(crate) mod normalized_field_selection {
    use crate::error::FederationError;
    use crate::query_plan::operation::{
        directives_with_sorted_arguments, is_interface_object, HasNormalizedSelectionKey,
        NormalizedSelectionKey, NormalizedSelectionSet, RebaseErrorHandlingOption, TYPENAME_FIELD,
    };
    use crate::schema::position::{
        CompositeTypeDefinitionPosition, FieldDefinitionPosition, TypeDefinitionPosition,
    };
    use crate::schema::ValidFederationSchema;
    use apollo_compiler::ast::{Argument, DirectiveList, Name};
    use apollo_compiler::Node;
    use std::sync::Arc;

    /// An analogue of the apollo-compiler type `Field` with these changes:
    /// - Makes the selection set optional. This is because `NormalizedSelectionSet` requires a type of
    ///   `CompositeTypeDefinitionPosition`, which won't exist for fields returning a non-composite type
    ///   (scalars and enums).
    /// - Stores the field data (other than the selection set) in `NormalizedField`, to facilitate
    ///   operation paths and graph paths.
    /// - For the field definition, stores the schema and the position in that schema instead of just
    ///   the `FieldDefinition` (which contains no references to the parent type or schema).
    /// - Encloses collection types in `Arc`s to facilitate cheaper cloning.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct NormalizedFieldSelection {
        pub(crate) field: NormalizedField,
        pub(crate) selection_set: Option<NormalizedSelectionSet>,
        pub(crate) sibling_typename: Option<Name>,
    }

    impl HasNormalizedSelectionKey for NormalizedFieldSelection {
        fn key(&self) -> NormalizedSelectionKey {
            self.field.key()
        }
    }

    /// The non-selection-set data of `NormalizedFieldSelection`, used with operation paths and graph
    /// paths.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(crate) struct NormalizedField {
        data: NormalizedFieldData,
        key: NormalizedSelectionKey,
    }

    impl NormalizedField {
        pub(crate) fn new(data: NormalizedFieldData) -> Self {
            Self {
                key: data.key(),
                data,
            }
        }

        pub(crate) fn data(&self) -> &NormalizedFieldData {
            &self.data
        }

        pub(crate) fn rebase_on(
            &self,
            parent_type: &CompositeTypeDefinitionPosition,
            schema: &ValidFederationSchema,
            error_handling: &RebaseErrorHandlingOption,
        ) -> Result<Option<NormalizedField>, FederationError> {
            let field_parent = self.data().field_position.parent();
            if field_parent == *parent_type {
                return Ok(Some(self.clone()));
            }

            if self.data().name() == &TYPENAME_FIELD {
                // TODO this info should be precomputed and available in schema.metadata
                return if schema
                    .possible_runtime_types(parent_type.clone())?
                    .iter()
                    .any(|t| is_interface_object(t, schema))
                {
                    if let RebaseErrorHandlingOption::ThrowError = error_handling {
                        Err(FederationError::internal(
                            format!("Cannot add selection of field \"{}\" to selection set of parent type \"{}\" that is potentially an interface object type at runtime",
                                    self.data().field_position,
                                    parent_type
                            )))
                    } else {
                        Ok(None)
                    }
                } else {
                    let mut updated_field_data = self.data().clone();
                    updated_field_data.field_position = parent_type.introspection_typename_field();
                    Ok(Some(NormalizedField::new(updated_field_data)))
                };
            }

            let parent_field = parent_type.field(self.data().name().clone())?;
            return if self.can_rebase_on(parent_type) {
                let mut updated_field_data = self.data().clone();
                updated_field_data.field_position = parent_field;
                Ok(Some(NormalizedField::new(updated_field_data)))
            } else if let RebaseErrorHandlingOption::IgnoreError = error_handling {
                Ok(None)
            } else {
                Err(FederationError::internal(format!(
                    "Cannot add selection of field \"{}\" to selection set of parent type \"{}\"",
                    self.data().field_position,
                    parent_type
                )))
            };
        }

        /// Verifies whether given field can be rebase on following parent type.
        ///
        /// There are 2 valid cases we want to allow:
        /// 1. either `parent_type` and `field_parent_type` are the same underlying type (same name) but from different underlying schema. Typically,
        ///  happens when we're building subgraph queries but using selections from the original query which is against the supergraph API schema.
        /// 2. or they are not the same underlying type, but the field parent type is from an interface (or an interface object, which is the same
        ///  here), in which case we may be rebasing an interface field on one of the implementation type, which is ok. Note that we don't verify
        ///  that `parent_type` is indeed an implementation of `field_parent_type` because it's possible that this implementation relationship exists
        ///  in the supergraph, but not in any of the subgraph schema involved here. So we just let it be. Not that `rebase_on` will complain anyway
        ///  if the field name simply does not exists in `parent_type`.
        fn can_rebase_on(&self, parent_type: &CompositeTypeDefinitionPosition) -> bool {
            let field_parent_type = self.data().field_position.parent();
            return field_parent_type.type_name() == parent_type.type_name()
                || field_parent_type.is_interface_type()
                || field_parent_type.is_union_type();
        }
    }

    impl HasNormalizedSelectionKey for NormalizedField {
        fn key(&self) -> NormalizedSelectionKey {
            self.key.clone()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(crate) struct NormalizedFieldData {
        pub(crate) schema: ValidFederationSchema,
        pub(crate) field_position: FieldDefinitionPosition,
        pub(crate) alias: Option<Name>,
        pub(crate) arguments: Arc<Vec<Node<Argument>>>,
        pub(crate) directives: Arc<DirectiveList>,
    }

    impl NormalizedFieldData {
        pub(crate) fn name(&self) -> &Name {
            self.field_position.field_name()
        }

        pub(crate) fn response_name(&self) -> Name {
            self.alias.clone().unwrap_or_else(|| self.name().clone())
        }

        pub(crate) fn is_leaf(&self) -> Result<bool, FederationError> {
            let definition = self.field_position.get(self.schema.schema())?;
            let base_type_position = self
                .schema
                .get_type(definition.ty.inner_named_type().clone())?;
            Ok(matches!(
                base_type_position,
                TypeDefinitionPosition::Scalar(_) | TypeDefinitionPosition::Enum(_)
            ))
        }
    }

    impl HasNormalizedSelectionKey for NormalizedFieldData {
        fn key(&self) -> NormalizedSelectionKey {
            NormalizedSelectionKey::Field {
                response_name: self.response_name(),
                directives: Arc::new(directives_with_sorted_arguments(&self.directives)),
            }
        }
    }
}

pub(crate) mod normalized_fragment_spread_selection {
    use crate::query_plan::operation::{
        directives_with_sorted_arguments, is_deferred_selection, HasNormalizedSelectionKey,
        NormalizedSelectionKey, NormalizedSelectionSet, SelectionId,
    };
    use crate::schema::position::CompositeTypeDefinitionPosition;
    use crate::schema::ValidFederationSchema;
    use apollo_compiler::ast::{DirectiveList, Name};
    use std::sync::Arc;

    /// An analogue of the apollo-compiler type `FragmentSpread` with these changes:
    /// - Stores the schema (may be useful for directives).
    /// - Encloses collection types in `Arc`s to facilitate cheaper cloning.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct NormalizedFragmentSpreadSelection {
        pub(crate) fragment_spread: NormalizedFragmentSpread,
        pub(crate) selection_set: NormalizedSelectionSet,
    }

    impl HasNormalizedSelectionKey for NormalizedFragmentSpreadSelection {
        fn key(&self) -> NormalizedSelectionKey {
            self.fragment_spread.key.clone()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(crate) struct NormalizedFragmentSpread {
        pub(crate) data: NormalizedFragmentSpreadData,
        pub(crate) key: NormalizedSelectionKey,
    }

    impl NormalizedFragmentSpread {
        pub(crate) fn new(data: NormalizedFragmentSpreadData) -> Self {
            Self {
                key: data.key(),
                data,
            }
        }

        pub(crate) fn data(&self) -> &NormalizedFragmentSpreadData {
            &self.data
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(crate) struct NormalizedFragmentSpreadData {
        pub(crate) schema: ValidFederationSchema,
        pub(crate) fragment_name: Name,
        pub(crate) directives: Arc<DirectiveList>,
        pub(crate) selection_id: SelectionId,
        spread_directives: Arc<DirectiveList>,
        type_condition_position: CompositeTypeDefinitionPosition,
    }
    //     pub(crate) type_condition_position: CompositeTypeDefinitionPosition,
    //     pub(crate) directives: Arc<DirectiveList>,
    //     pub(crate) selection_set: NormalizedSelectionSet,

    impl HasNormalizedSelectionKey for NormalizedFragmentSpreadData {
        fn key(&self) -> NormalizedSelectionKey {
            if is_deferred_selection(&self.directives) {
                NormalizedSelectionKey::DeferredFragmentSpread {
                    deferred_id: self.selection_id.clone(),
                }
            } else {
                NormalizedSelectionKey::FragmentSpread {
                    name: self.fragment_name.clone(),
                    directives: Arc::new(directives_with_sorted_arguments(&self.directives)),
                }
            }
        }
    }
}

pub(crate) mod normalized_inline_fragment_selection {
    use crate::error::FederationError;
    use crate::query_plan::operation::{
        directives_with_sorted_arguments, is_deferred_selection, print_possible_runtimes,
        runtime_types_intersect, HasNormalizedSelectionKey, NormalizedSelectionKey,
        NormalizedSelectionSet, RebaseErrorHandlingOption, SelectionId,
    };
    use crate::schema::position::CompositeTypeDefinitionPosition;
    use crate::schema::ValidFederationSchema;
    use apollo_compiler::ast::DirectiveList;
    use std::sync::Arc;

    /// An analogue of the apollo-compiler type `InlineFragment` with these changes:
    /// - Stores the inline fragment data (other than the selection set) in `NormalizedInlineFragment`,
    ///   to facilitate operation paths and graph paths.
    /// - For the type condition, stores the schema and the position in that schema instead of just
    ///   the `NamedType`.
    /// - Stores the parent type explicitly, which means storing the position (in apollo-compiler, this
    ///   is in the parent selection set).
    /// - Encloses collection types in `Arc`s to facilitate cheaper cloning.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct NormalizedInlineFragmentSelection {
        pub(crate) inline_fragment: NormalizedInlineFragment,
        pub(crate) selection_set: NormalizedSelectionSet,
    }

    impl HasNormalizedSelectionKey for NormalizedInlineFragmentSelection {
        fn key(&self) -> NormalizedSelectionKey {
            self.inline_fragment.key()
        }
    }

    /// The non-selection-set data of `NormalizedInlineFragmentSelection`, used with operation paths and
    /// graph paths.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(crate) struct NormalizedInlineFragment {
        data: NormalizedInlineFragmentData,
        key: NormalizedSelectionKey,
    }

    impl NormalizedInlineFragment {
        pub(crate) fn new(data: NormalizedInlineFragmentData) -> Self {
            Self {
                key: data.key(),
                data,
            }
        }

        pub(crate) fn data(&self) -> &NormalizedInlineFragmentData {
            &self.data
        }

        pub(crate) fn rebase_on(
            &self,
            parent_type: &CompositeTypeDefinitionPosition,
            schema: &ValidFederationSchema,
            error_handling: &RebaseErrorHandlingOption,
        ) -> Result<Option<NormalizedInlineFragment>, FederationError> {
            if &self.data.parent_type_position == parent_type {
                return Ok(Some(self.clone()));
            }

            let type_condition = self.data.type_condition_position.clone();
            // This usually imply that the fragment is not from the same sugraph than then selection. So we need
            // to update the source type of the fragment, but also "rebase" the condition to the selection set
            // schema.
            let (can_rebase, rebased_condition) = self.can_rebase_on(parent_type, schema);
            if !can_rebase {
                if let RebaseErrorHandlingOption::ThrowError = error_handling {
                    let printable_type_condition = self
                        .data
                        .type_condition_position
                        .clone()
                        .map_or_else(|| "".to_string(), |t| t.to_string());
                    let printable_runtimes = type_condition.map_or_else(
                        || "undefined".to_string(),
                        |t| print_possible_runtimes(&t, schema),
                    );
                    let printable_parent_runtimes = print_possible_runtimes(parent_type, schema);
                    Err(FederationError::internal(
                        format!("Cannot add fragment of condition \"{}\" (runtimes: [{}]) to parent type \"{}\" (runtimes: [{})",
                                printable_type_condition,
                                printable_runtimes,
                                parent_type,
                                printable_parent_runtimes,
                        ),
                    ))
                } else {
                    Ok(None)
                }
            } else {
                let mut rebased_fragment_data = self.data.clone();
                rebased_fragment_data.type_condition_position = rebased_condition;
                Ok(Some(NormalizedInlineFragment::new(rebased_fragment_data)))
            }
        }

        pub(crate) fn can_rebase_on(
            &self,
            parent_type: &CompositeTypeDefinitionPosition,
            parent_schema: &ValidFederationSchema,
        ) -> (bool, Option<CompositeTypeDefinitionPosition>) {
            if self.data.type_condition_position.is_none() {
                // can_rebase = true, condition = undefined
                return (true, None);
            }

            if let Some(Ok(rebased_condition)) = self
                .data
                .type_condition_position
                .clone()
                .and_then(|condition_position| {
                    parent_schema.try_get_type(condition_position.type_name().clone())
                })
                .map(|rebased_condition_position| {
                    CompositeTypeDefinitionPosition::try_from(rebased_condition_position)
                })
            {
                // chained if let chains are not yet supported
                // see https://github.com/rust-lang/rust/issues/53667
                if runtime_types_intersect(parent_type, &rebased_condition, parent_schema) {
                    // can_rebase = true, condition = rebased_condition
                    (true, Some(rebased_condition))
                } else {
                    (false, None)
                }
            } else {
                // can_rebase = false, condition = undefined
                (false, None)
            }
        }
    }

    impl HasNormalizedSelectionKey for NormalizedInlineFragment {
        fn key(&self) -> NormalizedSelectionKey {
            self.key.clone()
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(crate) struct NormalizedInlineFragmentData {
        pub(crate) schema: ValidFederationSchema,
        pub(crate) parent_type_position: CompositeTypeDefinitionPosition,
        pub(crate) type_condition_position: Option<CompositeTypeDefinitionPosition>,
        pub(crate) directives: Arc<DirectiveList>,
        pub(crate) selection_id: SelectionId,
    }

    impl HasNormalizedSelectionKey for NormalizedInlineFragmentData {
        fn key(&self) -> NormalizedSelectionKey {
            if is_deferred_selection(&self.directives) {
                NormalizedSelectionKey::DeferredInlineFragment {
                    deferred_id: self.selection_id.clone(),
                }
            } else {
                NormalizedSelectionKey::InlineFragment {
                    type_condition: self
                        .type_condition_position
                        .as_ref()
                        .map(|pos| pos.type_name().clone()),
                    directives: Arc::new(directives_with_sorted_arguments(&self.directives)),
                }
            }
        }
    }
}

/// Available fragment spread normalization options
pub(crate) enum FragmentSpreadNormalizationOption {
    InlineFragmentSpread,
    PreserveFragmentSpread,
}

impl NormalizedSelectionSet {
    pub(crate) fn empty(
        schema: ValidFederationSchema,
        type_position: CompositeTypeDefinitionPosition,
    ) -> Self {
        Self {
            schema,
            type_position,
            selections: Default::default(),
        }
    }

    pub(crate) fn contains_top_level_field(
        &self,
        field: &NormalizedField,
    ) -> Result<bool, FederationError> {
        if let Some(selection) = self.selections.get(&field.key()) {
            let NormalizedSelection::Field(field_selection) = selection else {
                return Err(Internal {
                    message: format!(
                        "Field selection key for field \"{}\" references non-field selection",
                        field.data().field_position,
                    ),
                }
                .into());
            };
            Ok(field_selection.field == *field)
        } else {
            Ok(false)
        }
    }

    /// Normalize this selection set (merging selections with the same keys), with the following
    /// additional transformations:
    /// - Expand fragment spreads into inline fragments.
    /// - Remove `__schema` or `__type` introspection fields, as these shouldn't be handled by query
    ///   planning.
    /// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
    ///   their parent type matches.
    ///
    /// Note this function asserts that the type of the selection set is a composite type (i.e. this
    /// isn't the empty selection set of some leaf field), and will return error if this is not the
    /// case.
    pub(crate) fn normalize_and_expand_fragments(
        selection_set: &SelectionSet,
        fragments: &IndexMap<Name, Node<Fragment>>,
        schema: &ValidFederationSchema,
        normalize_fragment_spread_option: &FragmentSpreadNormalizationOption,
    ) -> Result<NormalizedSelectionSet, FederationError> {
        let type_position: CompositeTypeDefinitionPosition =
            schema.get_type(selection_set.ty.clone())?.try_into()?;
        let mut normalized_selections = vec![];
        NormalizedSelectionSet::normalize_selections(
            &selection_set.selections,
            &type_position,
            &mut normalized_selections,
            fragments,
            schema,
            normalize_fragment_spread_option,
        )?;
        let mut merged = NormalizedSelectionSet {
            schema: schema.clone(),
            type_position,
            selections: Arc::new(NormalizedSelectionMap::new()),
        };
        merged.merge_selections_into(normalized_selections.into_iter())?;
        Ok(merged)
    }

    /// A helper function for normalizing a list of selections into a destination.
    fn normalize_selections(
        selections: &[Selection],
        parent_type_position: &CompositeTypeDefinitionPosition,
        destination: &mut Vec<NormalizedSelection>,
        fragments: &IndexMap<Name, Node<Fragment>>,
        schema: &ValidFederationSchema,
        normalize_fragment_spread_option: &FragmentSpreadNormalizationOption,
    ) -> Result<(), FederationError> {
        for selection in selections {
            match selection {
                Selection::Field(field_selection) => {
                    let Some(normalized_field_selection) =
                        NormalizedFieldSelection::normalize_and_expand_fragments(
                            field_selection,
                            parent_type_position,
                            fragments,
                            schema,
                            normalize_fragment_spread_option,
                        )?
                    else {
                        continue;
                    };
                    destination.push(NormalizedSelection::Field(Arc::new(
                        normalized_field_selection,
                    )));
                }
                Selection::FragmentSpread(fragment_spread_selection) => {
                    let Some(fragment) = fragments.get(&fragment_spread_selection.fragment_name)
                    else {
                        return Err(Internal {
                            message: format!(
                                "Fragment spread referenced non-existent fragment \"{}\"",
                                fragment_spread_selection.fragment_name,
                            ),
                        }
                        .into());
                    };
                    if let FragmentSpreadNormalizationOption::InlineFragmentSpread =
                        normalize_fragment_spread_option
                    {
                        // We can hoist/collapse named fragments if their type condition is on the
                        // parent type and they don't have any directives.
                        if fragment.type_condition() == parent_type_position.type_name()
                            && fragment_spread_selection.directives.is_empty()
                        {
                            NormalizedSelectionSet::normalize_selections(
                                &fragment.selection_set.selections,
                                parent_type_position,
                                destination,
                                fragments,
                                schema,
                                normalize_fragment_spread_option,
                            )?;
                        } else {
                            let normalized_inline_fragment_selection =
                                NormalizedFragmentSpreadSelection::normalize_and_expand_fragments(
                                    fragment_spread_selection,
                                    parent_type_position,
                                    fragments,
                                    schema,
                                    normalize_fragment_spread_option,
                                )?;
                            destination.push(NormalizedSelection::InlineFragment(Arc::new(
                                normalized_inline_fragment_selection,
                            )));
                        }
                    } else {
                        // if we don't expand fragments, we just convert FragmentSpread to NormalizedFragmentSpreadSelection
                        let normalized_fragment_spread =
                            NormalizedFragmentSpreadSelection::normalize(
                                fragment,
                                fragment_spread_selection,
                                schema,
                            );
                        destination.push(NormalizedSelection::FragmentSpread(Arc::new(
                            normalized_fragment_spread,
                        )));
                    }
                }
                Selection::InlineFragment(inline_fragment_selection) => {
                    let is_on_parent_type =
                        if let Some(type_condition) = &inline_fragment_selection.type_condition {
                            type_condition == parent_type_position.type_name()
                        } else {
                            true
                        };
                    // We can hoist/collapse inline fragments if their type condition is on the
                    // parent type (or they have no type condition) and they don't have any
                    // directives.
                    //
                    // PORT_NOTE: The JS codebase didn't hoist inline fragments, only fragment
                    // spreads (presumably because named fragments would commonly be on the same
                    // type as their fragment spread usages). It should be fine to also hoist inline
                    // fragments though if we notice they're similarly useless (and presumably later
                    // transformations in the JS codebase would take care of this).
                    if is_on_parent_type && inline_fragment_selection.directives.is_empty() {
                        NormalizedSelectionSet::normalize_selections(
                            &inline_fragment_selection.selection_set.selections,
                            parent_type_position,
                            destination,
                            fragments,
                            schema,
                            normalize_fragment_spread_option,
                        )?;
                    } else {
                        let normalized_inline_fragment_selection =
                            NormalizedInlineFragmentSelection::normalize_and_expand_fragments(
                                inline_fragment_selection,
                                parent_type_position,
                                fragments,
                                schema,
                                normalize_fragment_spread_option,
                            )?;
                        destination.push(NormalizedSelection::InlineFragment(Arc::new(
                            normalized_inline_fragment_selection,
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Merges the given normalized selection sets into this one.
    pub(crate) fn merge_into(
        &mut self,
        others: impl Iterator<Item = NormalizedSelectionSet> + ExactSizeIterator,
    ) -> Result<(), FederationError> {
        if others.len() > 0 {
            let mut selections_to_merge = vec![];
            for other in others {
                if other.schema != self.schema {
                    return Err(Internal {
                        message: "Cannot merge selection sets from different schemas".to_owned(),
                    }
                    .into());
                }
                if other.type_position != self.type_position {
                    return Err(Internal {
                        message: format!(
                            "Cannot merge selection set for type \"{}\" into a selection set for type \"{}\"",
                            other.type_position,
                            self.type_position,
                        ),
                    }.into());
                }
                let selections = Arc::try_unwrap(other.selections)
                    .unwrap_or_else(|selections| selections.deref().clone());
                for (_, value) in selections {
                    selections_to_merge.push(value);
                }
            }
            self.merge_selections_into(selections_to_merge.into_iter())?;
        }
        Ok(())
    }

    /// A helper function for merging the given selections into this one.
    fn merge_selections_into(
        &mut self,
        others: impl Iterator<Item = NormalizedSelection> + ExactSizeIterator,
    ) -> Result<(), FederationError> {
        if others.len() > 0 {
            let mut fields = IndexMap::new();
            let mut fragment_spreads = IndexMap::new();
            let mut inline_fragments = IndexMap::new();
            for other_selection in others {
                let other_key = other_selection.key();
                match Arc::make_mut(&mut self.selections).entry(other_key.clone()) {
                    Entry::Occupied(existing) => match existing.get() {
                        NormalizedSelection::Field(self_field_selection) => {
                            let NormalizedSelection::Field(other_field_selection) = other_selection
                            else {
                                return Err(Internal {
                                        message: format!(
                                            "Field selection key for field \"{}\" references non-field selection",
                                            self_field_selection.field.data().field_position,
                                        ),
                                    }.into());
                            };
                            let other_field_selection = Arc::try_unwrap(other_field_selection)
                                .unwrap_or_else(|selection| selection.deref().clone());
                            fields
                                .entry(other_key)
                                .or_insert_with(Vec::new)
                                .push(other_field_selection);
                        }
                        NormalizedSelection::FragmentSpread(self_fragment_spread_selection) => {
                            let NormalizedSelection::FragmentSpread(
                                other_fragment_spread_selection,
                            ) = other_selection
                            else {
                                return Err(Internal {
                                        message: format!(
                                            "Fragment spread selection key for fragment \"{}\" references non-field selection",
                                            self_fragment_spread_selection.fragment_spread.data().fragment_name,
                                        ),
                                    }.into());
                            };
                            let other_fragment_spread_selection =
                                Arc::try_unwrap(other_fragment_spread_selection)
                                    .unwrap_or_else(|selection| selection.deref().clone());
                            fragment_spreads
                                .entry(other_key)
                                .or_insert_with(Vec::new)
                                .push(other_fragment_spread_selection);
                        }
                        NormalizedSelection::InlineFragment(self_inline_fragment_selection) => {
                            let NormalizedSelection::InlineFragment(
                                other_inline_fragment_selection,
                            ) = other_selection
                            else {
                                return Err(Internal {
                                        message: format!(
                                            "Inline fragment selection key under parent type \"{}\" {}references non-field selection",
                                            self_inline_fragment_selection.inline_fragment.data().parent_type_position,
                                            self_inline_fragment_selection.inline_fragment.data().type_condition_position.clone()
                                                .map_or_else(
                                                    String::new,
                                                    |cond| format!("(type condition: {}) ", cond),
                                                ),
                                        ),
                                    }.into());
                            };
                            let other_inline_fragment_selection =
                                Arc::try_unwrap(other_inline_fragment_selection)
                                    .unwrap_or_else(|selection| selection.deref().clone());
                            inline_fragments
                                .entry(other_key)
                                .or_insert_with(Vec::new)
                                .push(other_inline_fragment_selection);
                        }
                    },
                    Entry::Vacant(vacant) => {
                        vacant.insert(other_selection)?;
                    }
                }
            }
            for (key, self_selection) in Arc::make_mut(&mut self.selections).iter_mut() {
                match self_selection {
                    NormalizedSelectionValue::Field(mut self_field_selection) => {
                        if let Some(other_field_selections) = fields.remove(key) {
                            self_field_selection.merge_into(other_field_selections.into_iter())?;
                        }
                    }
                    NormalizedSelectionValue::FragmentSpread(
                        mut self_fragment_spread_selection,
                    ) => {
                        if let Some(other_fragment_spread_selections) = fragment_spreads.remove(key)
                        {
                            self_fragment_spread_selection
                                .merge_into(other_fragment_spread_selections.into_iter())?;
                        }
                    }
                    NormalizedSelectionValue::InlineFragment(
                        mut self_inline_fragment_selection,
                    ) => {
                        if let Some(other_inline_fragment_selections) = inline_fragments.remove(key)
                        {
                            self_inline_fragment_selection
                                .merge_into(other_inline_fragment_selections.into_iter())?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Modifies the provided selection set to optimize the handling of __typename selections for query planning.
    ///
    /// __typename information can always be provided by any subgraph declaring that type. While this data can be
    /// theoretically fetched from multiple sources, in practice it doesn't really matter which subgraph we use
    /// for the __typename and we should just get it from the same source as the one that was used to resolve
    /// other fields.
    ///
    /// In most cases, selecting __typename won't be a problem as query planning algorithm ignores "obviously"
    /// inefficient paths. Typically, querying the __typename of an entity is generally ok because when looking at
    /// a path, the query planning algorithm always favor getting a field "locally" if it can (which it always can
    /// for __typename) and ignore alternative that would jump subgraphs.
    ///
    /// When querying a __typename after a @shareable field, query planning algorithm would consider getting the
    /// __typename from EACH version of the @shareable field. This unnecessarily explodes the number of possible
    /// query plans with some useless options and results in degraded performance. Since the number of possible
    /// plans doubles for every field for which there is a choice, eliminating unnecessary choices improves query
    /// planning performance.
    ///
    /// It is unclear how to do this cleanly with the current planning algorithm, so this method is a workaround
    /// so we can efficiently generate query plans. In order to prevent the query planner from spending time
    /// exploring those useless __typename options, we "remove" the unnecessary __typename selections from the
    /// operation. Since we need to ensure that the __typename field will still need to be queried, we "tag"
    /// one of the "sibling" selections (using "attachement") to remember that __typename needs to be added
    /// back eventually. The core query planning algorithm will ignore that tag, and because __typename has been
    /// otherwise removed, we'll save any related work. As we build the final query plan, we'll check back for
    /// those "tags" and add back the __typename selections. As this only happen after the query planning
    /// algorithm has computed all choices, we achieve our goal of not considering useless choices due to
    /// __typename. Do note that if __typename is the "only" selection of some selection set, then we leave it
    /// untouched, and let the query planning algorithm treat it as any other field. We have no other choice in
    /// that case, and that's actually what we want.
    pub(crate) fn optimize_sibling_typenames(
        &mut self,
        interface_types_with_interface_objects: &IndexSet<InterfaceTypeDefinitionPosition>,
    ) -> Result<(), FederationError> {
        let is_interface_object =
            interface_types_with_interface_objects.contains(&InterfaceTypeDefinitionPosition {
                type_name: self.type_position.type_name().clone(),
            });
        let mut typename_field_key: Option<NormalizedSelectionKey> = None;
        let mut sibling_field_key: Option<NormalizedSelectionKey> = None;

        let mutable_selection_map = Arc::make_mut(&mut self.selections);
        for (key, entry) in mutable_selection_map.iter_mut() {
            match entry {
                NormalizedSelectionValue::Field(mut field_selection) => {
                    if field_selection.get().field.data().name() == &TYPENAME_FIELD
                        && !is_interface_object
                        && typename_field_key.is_none()
                    {
                        typename_field_key = Some(key.clone());
                    } else if sibling_field_key.is_none() {
                        sibling_field_key = Some(key.clone());
                    }

                    if let Some(field_selection_set) = field_selection.get_selection_set_mut() {
                        field_selection_set
                            .optimize_sibling_typenames(interface_types_with_interface_objects)?;
                    }
                }
                NormalizedSelectionValue::InlineFragment(mut inline_fragment) => {
                    inline_fragment
                        .get_selection_set_mut()
                        .optimize_sibling_typenames(interface_types_with_interface_objects)?;
                }
                NormalizedSelectionValue::FragmentSpread(fragment_spread) => {
                    // at this point in time all fragment spreads should have been converted into inline fragments
                    return Err(FederationError::SingleFederationError(Internal {
                        message: format!(
                            "Error while optimizing sibling typename information, selection set contains {} named fragment",
                            fragment_spread.get().fragment_spread.data().fragment_name
                        ),
                    }));
                }
            }
        }

        if let (Some(typename_key), Some(sibling_field_key)) =
            (typename_field_key, sibling_field_key)
        {
            if let (
                Some(NormalizedSelection::Field(typename_field)),
                Some(NormalizedSelectionValue::Field(mut sibling_field)),
            ) = (
                mutable_selection_map.remove(&typename_key),
                mutable_selection_map.get_mut(&sibling_field_key),
            ) {
                *sibling_field.get_sibling_typename_mut() =
                    Some(typename_field.field.data().response_name());
            } else {
                unreachable!("typename and sibling fields must both exist at this point")
            }
        }
        Ok(())
    }

    pub(crate) fn conditions(&self) -> Result<Conditions, FederationError> {
        // If the conditions of all the selections within the set are the same,
        // then those are conditions of the whole set and we return it.
        // Otherwise, we just return `true`
        // (which essentially translate to "that selection always need to be queried").
        // Note that for the case where the set has only 1 selection,
        // then this just mean we return the condition of that one selection.
        // Also note that in theory we could be a tad more precise,
        // and when all the selections have variable conditions,
        // we could return the intersection of all of them,
        // but we don't bother for now as that has probably extremely rarely an impact in practice.
        let mut selections = self.selections.values();
        let Some(first_selection) = selections.next() else {
            // we shouldn't really get here for well-formed selection, so whether we return true or false doesn't matter
            // too much, but in principle, if there is no selection, we should be cool not including it.
            return Ok(Conditions::Boolean(false));
        };
        let conditions = first_selection.conditions()?;
        for selection in selections {
            if selection.conditions()? != conditions {
                return Ok(Conditions::Boolean(true));
            }
        }
        Ok(conditions)
    }

    pub(crate) fn add_back_typename_in_attachments(
        &self,
    ) -> Result<NormalizedSelectionSet, FederationError> {
        todo!()
    }

    pub(crate) fn add_typename_field_for_abstract_types(
        &self,
    ) -> Result<NormalizedSelectionSet, FederationError> {
        todo!()
    }

    pub(crate) fn rebase_on(
        &self,
        parent_type: &CompositeTypeDefinitionPosition,
        fragments: &NamedFragments,
        schema: &ValidFederationSchema,
        error_handling: &RebaseErrorHandlingOption,
    ) -> Result<NormalizedSelectionSet, FederationError> {
        let mut rebased_selections = NormalizedSelectionMap::new();
        let rebased_results: Result<Vec<Option<NormalizedSelection>>, FederationError> = self
            .selections
            .iter()
            .map(|(_, selection)| match selection {
                NormalizedSelection::Field(field) => {
                    field.rebase_on(parent_type, fragments, schema, error_handling)
                }
                NormalizedSelection::FragmentSpread(spread) => {
                    spread.rebase_on(parent_type, fragments, schema, error_handling)
                }
                NormalizedSelection::InlineFragment(inline) => {
                    inline.rebase_on(parent_type, fragments, schema, error_handling)
                }
            })
            .collect();
        rebased_results?.iter().flatten().for_each(|rebased| {
            rebased_selections.insert(rebased.clone());
        });

        Ok(NormalizedSelectionSet {
            schema: self.schema.clone(),
            type_position: self.type_position.clone(),
            selections: Arc::new(rebased_selections),
        })
    }

    /// Applies some normalization rules to this selection set in the context of the provided `parent_type`.
    ///
    /// Normalization mostly removes unnecessary/redundant inline fragments, so that for instance, with a schema:
    /// ```graphql
    /// type Query {
    ///   t1: T1
    ///   i: I
    /// }
    ///
    /// interface I {
    ///   id: ID!
    /// }
    ///
    /// type T1 implements I {
    ///   id: ID!
    ///   v1: Int
    /// }
    ///
    /// type T2 implements I {
    ///   id: ID!
    ///   v2: Int
    /// }
    /// ```
    /// We can perform following normalization
    /// ```
    /// normalize({
    ///   t1 {
    ///     ... on I {
    ///       id
    ///     }
    ///   }
    ///   i {
    ///     ... on T1 {
    ///       ... on I {
    ///         ... on T1 {
    ///           v1
    ///         }
    ///         ... on T2 {
    ///           v2
    ///         }
    ///       }
    ///     }
    ///     ... on T2 {
    ///       ... on I {
    ///         id
    ///       }
    ///     }
    ///   }
    /// }) === {
    ///   t1 {
    ///     id
    ///   }
    ///   i {
    ///     ... on T1 {
    ///       v1
    ///     }
    ///     ... on T2 {
    ///       id
    ///     }
    ///   }
    /// }
    /// ```
    ///
    /// For this operation to be valid (to not throw), `parent_type` must be such that every field selection in
    /// this selection set is such that its type position intersects with passed `parent_type` (there is no limitation
    /// on the fragment selections, though any fragment selections whose condition do not intersects `parent_type`
    /// will be discarded). Note that `self.normalize(self.type_condition)` is always valid and useful, but it is
    /// also possible to pass a `parent_type` that is more "restrictive" than the selection current type position
    /// (as long as the top-level fields of this selection set can be rebased on that type).
    ///
    /// Passing the option `recursive == false` makes the normalization only apply at the top-level, removing
    /// any unnecessary top-level inline fragments, possibly multiple layers of them, but we never recurse
    /// inside the sub-selection of an selection that is not removed by the normalization.
    pub(crate) fn normalize(
        &mut self,
        parent_type: &CompositeTypeDefinitionPosition,
    ) -> NormalizedSelectionSet {
        let mut normalized_selection_map = NormalizedSelectionMap::new();
        self.selections.iter().for_each(|(_, s)| {
            let normalized = s.normalize(parent_type);
            normalized_selection_map.insert(normalized);
        });

        NormalizedSelectionSet {
            schema: self.schema.clone(),
            type_position: self.type_position.clone(),
            selections: Arc::new(normalized_selection_map),
        }
    }

    fn collect_used_fragment_names(&self, aggregator: &mut Arc<HashMap<Name, i32>>) {
        self.selections
            .iter()
            .for_each(|(_, s)| s.collect_used_fragment_names(aggregator));
    }
}

impl NormalizedFieldSelection {
    /// Normalize this field selection (merging selections with the same keys), with the following
    /// additional transformations:
    /// - Expand fragment spreads into inline fragments.
    /// - Remove `__schema` or `__type` introspection fields, as these shouldn't be handled by query
    ///   planning.
    /// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
    ///   their parent type matches.
    pub(crate) fn normalize_and_expand_fragments(
        field: &Field,
        parent_type_position: &CompositeTypeDefinitionPosition,
        fragments: &IndexMap<Name, Node<Fragment>>,
        schema: &ValidFederationSchema,
        normalize_fragment_spread_option: &FragmentSpreadNormalizationOption,
    ) -> Result<Option<NormalizedFieldSelection>, FederationError> {
        // Skip __schema/__type introspection fields as router takes care of those, and they do not
        // need to be query planned.
        if field.name == "__schema" || field.name == "__type" {
            return Ok(None);
        }
        let field_position = parent_type_position.field(field.name.clone())?;
        // We might be able to validate that the returned `FieldDefinition` matches that within
        // the given `field`, but on the off-chance there's a mutation somewhere in between
        // Operation creation and the creation of the ValidFederationSchema, it's safer to just
        // confirm it exists in this schema.
        field_position.get(schema.schema())?;
        let field_composite_type_result: Result<CompositeTypeDefinitionPosition, FederationError> =
            schema.get_type(field.selection_set.ty.clone())?.try_into();

        Ok(Some(NormalizedFieldSelection {
            field: NormalizedField::new(NormalizedFieldData {
                schema: schema.clone(),
                field_position,
                alias: field.alias.clone(),
                arguments: Arc::new(field.arguments.clone()),
                directives: Arc::new(field.directives.clone()),
            }),
            selection_set: if field_composite_type_result.is_ok() {
                Some(NormalizedSelectionSet::normalize_and_expand_fragments(
                    &field.selection_set,
                    fragments,
                    schema,
                    normalize_fragment_spread_option,
                )?)
            } else {
                None
            },
            sibling_typename: None,
        }))
    }

    /// Returns a field selection "equivalent" to the one represented by this object, but such that its parent type
    /// is the one provided as argument.
    ///
    /// Obviously, this operation will only succeed if this selection (both the field itself and its subselections)
    /// make sense from the provided parent type. If this is not the case, this method will throw.
    pub(crate) fn rebase_on(
        &self,
        parent_type: &CompositeTypeDefinitionPosition,
        fragments: &NamedFragments,
        schema: &ValidFederationSchema,
        error_handling: &RebaseErrorHandlingOption,
    ) -> Result<Option<NormalizedSelection>, FederationError> {
        if &self.field.data().field_position.parent() == parent_type {
            return Ok(Some(NormalizedSelection::Field(Arc::new(self.clone()))));
        }

        return if let Some(rebased) = self.field.rebase_on(parent_type, schema, error_handling)? {
            if let Some(selection_set) = &self.selection_set {
                let rebased_base_type_name = rebased.data().field_position.type_name();
                let rebased_type: CompositeTypeDefinitionPosition = schema
                    .get_type(rebased_base_type_name.clone())?
                    .try_into()?;
                let selection_set_type = &selection_set.type_position;
                if &rebased_type == selection_set_type {
                    return Ok(Some(NormalizedSelection::Field(Arc::new(
                        NormalizedFieldSelection {
                            field: rebased.clone(),
                            selection_set: self.selection_set.clone(),
                            sibling_typename: self.sibling_typename.clone(),
                        },
                    ))));
                }

                let rebased_selection_set =
                    selection_set.rebase_on(&rebased_type, fragments, schema, error_handling)?;
                if rebased_selection_set.selections.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(NormalizedSelection::Field(Arc::new(
                        NormalizedFieldSelection {
                            field: rebased.clone(),
                            selection_set: Some(rebased_selection_set),
                            sibling_typename: self.sibling_typename.clone(),
                        },
                    ))))
                }
            } else {
                Ok(Some(NormalizedSelection::Field(Arc::new(
                    NormalizedFieldSelection {
                        field: rebased,
                        selection_set: self.selection_set.clone(),
                        sibling_typename: self.sibling_typename.clone(),
                    },
                ))))
            }
        } else {
            Ok(None)
        };
    }
}

impl<'a> NormalizedFieldSelectionValue<'a> {
    /// Merges the given normalized field selections into this one (this method assumes the keys
    /// already match).
    pub(crate) fn merge_into(
        &mut self,
        others: impl Iterator<Item = NormalizedFieldSelection> + ExactSizeIterator,
    ) -> Result<(), FederationError> {
        if others.len() > 0 {
            let self_field = &self.get().field;
            let mut selection_sets = vec![];
            for other in others {
                let other_field = &other.field;
                if other_field.data().schema != self_field.data().schema {
                    return Err(Internal {
                        message: "Cannot merge field selections from different schemas".to_owned(),
                    }
                    .into());
                }
                if other_field.data().field_position != self_field.data().field_position {
                    return Err(Internal {
                        message: format!(
                            "Cannot merge field selection for field \"{}\" into a field selection for field \"{}\"",
                            other_field.data().field_position,
                            self_field.data().field_position,
                        ),
                    }.into());
                }
                if self.get().selection_set.is_some() {
                    let Some(other_selection_set) = other.selection_set else {
                        return Err(Internal {
                            message: format!(
                                "Field \"{}\" has composite type but not a selection set",
                                other_field.data().field_position,
                            ),
                        }
                        .into());
                    };
                    selection_sets.push(other_selection_set);
                } else if other.selection_set.is_some() {
                    return Err(Internal {
                        message: format!(
                            "Field \"{}\" has non-composite type but also has a selection set",
                            other_field.data().field_position,
                        ),
                    }
                    .into());
                }
            }
            if let Some(self_selection_set) = self.get_selection_set_mut() {
                self_selection_set.merge_into(selection_sets.into_iter())?;
            }
        }
        Ok(())
    }
}

impl NormalizedFragmentSpreadSelection {
    /// Copies fragment spread selection and assigns it a new unique selection ID.
    pub(crate) fn with_unique_id(&self) -> Self {
        let mut data = self.fragment_spread.data().clone();
        data.selection_id = SelectionId::new();
        Self {
            fragment_spread: NormalizedFragmentSpread::new(data),
            selection_set: self.selection_set.clone(),
        }
    }

    /// Normalize this fragment spread into a "normalized" spread representation with following
    /// modifications
    /// - Stores the schema (may be useful for directives).
    /// - Encloses list of directives in `Arc`s to facilitate cheaper cloning.
    /// - Stores unique selection ID (used for deferred fragments)
    pub(crate) fn normalize(
        fragment: &Node<NormalizedFragment>,
        fragment_spread: &FragmentSpread,
        schema: &ValidFederationSchema,
    ) -> NormalizedFragmentSpreadSelection {
        let data = NormalizedFragmentSpreadData {
            schema: schema.clone(),
            fragment_name: fragment_spread.fragment_name.clone(),
            directives: Arc::new(fragment_spread.directives.clone()),
            selection_id: SelectionId::new(),
        };

        NormalizedFragmentSpreadSelection {
            fragment_spread: NormalizedFragmentSpread::new(data),
            selection_set: fragment.selection_set.clone(),
        }
    }

    /// Normalize this fragment spread (merging selections with the same keys), with the following
    /// additional transformations:
    /// - Expand fragment spreads into inline fragments.
    /// - Remove `__schema` or `__type` introspection fields, as these shouldn't be handled by query
    ///   planning.
    /// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
    ///   their parent type matches.
    pub(crate) fn normalize_and_expand_fragments(
        fragment_spread: &FragmentSpread,
        parent_type_position: &CompositeTypeDefinitionPosition,
        fragments: &IndexMap<Name, Node<Fragment>>,
        schema: &ValidFederationSchema,
        normalize_fragment_spread_option: &FragmentSpreadNormalizationOption,
    ) -> Result<NormalizedInlineFragmentSelection, FederationError> {
        let Some(fragment) = fragments.get(&fragment_spread.fragment_name) else {
            return Err(Internal {
                message: format!(
                    "Fragment spread referenced non-existent fragment \"{}\"",
                    fragment_spread.fragment_name,
                ),
            }
            .into());
        };
        let type_condition_position: CompositeTypeDefinitionPosition = schema
            .get_type(fragment.type_condition().clone())?
            .try_into()?;

        // PORT_NOTE: The JS codebase combined the fragment spread's directives with the fragment
        // definition's directives. This was invalid GraphQL, so we're explicitly ignoring the
        // fragment definition's directives here (which isn't great, but there's not a simple
        // alternative at the moment).
        Ok(NormalizedInlineFragmentSelection {
            inline_fragment: NormalizedInlineFragment::new(NormalizedInlineFragmentData {
                schema: schema.clone(),
                parent_type_position: parent_type_position.clone(),
                type_condition_position: Some(type_condition_position),
                directives: Arc::new(fragment_spread.directives.clone()),
                selection_id: SelectionId::new(),
            }),
            selection_set: NormalizedSelectionSet::normalize_and_expand_fragments(
                &fragment.selection_set,
                fragments,
                schema,
                normalize_fragment_spread_option,
            )?,
        })
    }

    pub(crate) fn rebase_on(
        &self,
        parent_type: &CompositeTypeDefinitionPosition,
        fragments: &NamedFragments,
        schema: &ValidFederationSchema,
        error_handling: &RebaseErrorHandlingOption,
    ) -> Result<Option<NormalizedSelection>, FederationError> {
        // We preserve the parent type here, to make sure we don't lose context, but we actually don't
        // want to expand the spread  as that would compromise the code that optimize subgraph fetches to re-use named
        // fragments.
        //
        // This is a little bit iffy, because the fragment may not apply at this parent type, but we
        // currently leave it to the caller to ensure this is not a mistake. But most of the
        // QP code works on selections with fully expanded fragments, so this code (and that of `canAddTo`
        // on come into play in the code for reusing fragments, and that code calls those methods
        // appropriately.
        if self.parentType == parent_type {
            return Ok(Some(NormalizedSelection::FragmentSpread(Arc::new(
                self.clone(),
            ))));
        }

        todo!()
    }
}

impl<'a> NormalizedFragmentSpreadSelectionValue<'a> {
    /// Merges the given normalized fragment spread selections into this one (this method assumes
    /// the keys already match).
    pub(crate) fn merge_into(
        &mut self,
        others: impl Iterator<Item = NormalizedFragmentSpreadSelection> + ExactSizeIterator,
    ) -> Result<(), FederationError> {
        if others.len() > 0 {
            for other in others {
                if other.data().schema != self.get().data().schema {
                    return Err(Internal {
                        message: "Cannot merge fragment spread from different schemas".to_owned(),
                    }
                    .into());
                }
                // Nothing to do since the fragment spread is already part of the selection set.
                // Fragment spreads are uniquely identified by fragment name and applied directives.
                // Since there is already an entry for the same fragment spread, there is no point
                // in attempting to merge its sub-selections, as the underlying entry should be
                // exactly the same as the currently processed one.
            }
        }
        Ok(())
    }
}

impl NormalizedInlineFragmentSelection {
    /// Copies inline fragment selection and assigns it a new unique selection ID.
    pub(crate) fn with_unique_id(&self) -> Self {
        let mut data = self.inline_fragment.data().clone();
        data.selection_id = SelectionId::new();
        Self {
            inline_fragment: NormalizedInlineFragment::new(data),
            selection_set: self.selection_set.clone(),
        }
    }

    /// Normalize this inline fragment selection (merging selections with the same keys), with the
    /// following additional transformations:
    /// - Expand fragment spreads into inline fragments.
    /// - Remove `__schema` or `__type` introspection fields, as these shouldn't be handled by query
    ///   planning.
    /// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
    ///   their parent type matches.
    pub(crate) fn normalize_and_expand_fragments(
        inline_fragment: &InlineFragment,
        parent_type_position: &CompositeTypeDefinitionPosition,
        fragments: &IndexMap<Name, Node<Fragment>>,
        schema: &ValidFederationSchema,
        normalize_fragment_spread_option: &FragmentSpreadNormalizationOption,
    ) -> Result<NormalizedInlineFragmentSelection, FederationError> {
        let type_condition_position: Option<CompositeTypeDefinitionPosition> =
            if let Some(type_condition) = &inline_fragment.type_condition {
                Some(schema.get_type(type_condition.clone())?.try_into()?)
            } else {
                None
            };
        Ok(NormalizedInlineFragmentSelection {
            inline_fragment: NormalizedInlineFragment::new(NormalizedInlineFragmentData {
                schema: schema.clone(),
                parent_type_position: parent_type_position.clone(),
                type_condition_position,
                directives: Arc::new(inline_fragment.directives.clone()),
                selection_id: SelectionId::new(),
            }),
            selection_set: NormalizedSelectionSet::normalize_and_expand_fragments(
                &inline_fragment.selection_set,
                fragments,
                schema,
                normalize_fragment_spread_option,
            )?,
        })
    }

    pub(crate) fn rebase_on(
        &self,
        parent_type: &CompositeTypeDefinitionPosition,
        named_fragments: &NamedFragments,
        schema: &ValidFederationSchema,
        error_handling: &RebaseErrorHandlingOption,
    ) -> Result<Option<NormalizedSelection>, FederationError> {
        if &self.inline_fragment.data().parent_type_position == parent_type {
            return Ok(Some(NormalizedSelection::InlineFragment(Arc::new(
                self.clone(),
            ))));
        }
        return if let Some(rebased_fragment) =
            self.inline_fragment
                .rebase_on(parent_type, schema, error_handling)?
        {
            let rebased_casted_type = rebased_fragment
                .data()
                .type_condition_position
                .clone()
                .unwrap_or(rebased_fragment.data().parent_type_position.clone());
            if &rebased_casted_type == parent_type {
                Ok(Some(NormalizedSelection::InlineFragment(Arc::new(
                    NormalizedInlineFragmentSelection {
                        inline_fragment: rebased_fragment,
                        selection_set: self.selection_set.clone(),
                    },
                ))))
            } else {
                let rebased_selection_set = self.selection_set.rebase_on(
                    &rebased_casted_type,
                    named_fragments,
                    schema,
                    error_handling,
                )?;
                if rebased_selection_set.selections.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(NormalizedSelection::InlineFragment(Arc::new(
                        NormalizedInlineFragmentSelection {
                            inline_fragment: rebased_fragment,
                            selection_set: rebased_selection_set,
                        },
                    ))))
                }
            }
        } else {
            Ok(None)
        };
    }
}

impl<'a> NormalizedInlineFragmentSelectionValue<'a> {
    /// Merges the given normalized inline fragment selections into this one (this method assumes
    /// the keys already match).
    pub(crate) fn merge_into(
        &mut self,
        others: impl Iterator<Item = NormalizedInlineFragmentSelection> + ExactSizeIterator,
    ) -> Result<(), FederationError> {
        if others.len() > 0 {
            let self_inline_fragment = &self.get().inline_fragment;
            let mut selection_sets = vec![];
            for other in others {
                let other_inline_fragment = &other.inline_fragment;
                if other_inline_fragment.data().schema != self_inline_fragment.data().schema {
                    return Err(Internal {
                        message: "Cannot merge inline fragment from different schemas".to_owned(),
                    }
                    .into());
                }
                if other_inline_fragment.data().parent_type_position
                    != self_inline_fragment.data().parent_type_position
                {
                    return Err(Internal {
                        message: format!(
                            "Cannot merge inline fragment of parent type \"{}\" into an inline fragment of parent type \"{}\"",
                            other_inline_fragment.data().parent_type_position,
                            self_inline_fragment.data().parent_type_position,
                        ),
                    }.into());
                }
                selection_sets.push(other.selection_set);
            }
            self.get_selection_set_mut()
                .merge_into(selection_sets.into_iter())?;
        }
        Ok(())
    }
}

pub(crate) fn merge_selection_sets(
    mut selection_sets: impl Iterator<Item = NormalizedSelectionSet> + ExactSizeIterator,
) -> Result<NormalizedSelectionSet, FederationError> {
    let Some(mut first) = selection_sets.next() else {
        return Err(Internal {
            message: "".to_owned(),
        }
        .into());
    };
    first.merge_into(selection_sets)?;
    Ok(first)
}

pub(crate) fn equal_selection_sets(
    _a: &NormalizedSelectionSet,
    _b: &NormalizedSelectionSet,
) -> Result<bool, FederationError> {
    // TODO: Once operation processing is done, we should be able to call into that logic here.
    // We're specifically wanting the equivalent of something like
    // ```
    // selectionSetOfNode(...).equals(selectionSetOfNode(...));
    // ```
    // from the JS codebase. It may be more performant for federation-next to use its own
    // representation instead of repeatedly inter-converting between its representation and the
    // apollo-rs one, but we'll cross that bridge if we come to it.
    todo!();
}

impl TryFrom<&NormalizedOperation> for Operation {
    type Error = FederationError;

    fn try_from(normalized_operation: &NormalizedOperation) -> Result<Self, Self::Error> {
        let operation_type: OperationType = normalized_operation.root_kind.into();
        Ok(Self {
            operation_type,
            name: normalized_operation.name.clone(),
            variables: normalized_operation.variables.deref().clone(),
            directives: normalized_operation.directives.deref().clone(),
            selection_set: (&normalized_operation.selection_set).try_into()?,
        })
    }
}

impl TryFrom<&NormalizedSelectionSet> for SelectionSet {
    type Error = FederationError;

    fn try_from(val: &NormalizedSelectionSet) -> Result<Self, Self::Error> {
        let mut flattened = vec![];
        for normalized_selection in val.selections.values() {
            let selection: Selection = normalized_selection.try_into()?;
            flattened.push(selection);
        }
        Ok(Self {
            ty: val.type_position.type_name().clone(),
            selections: flattened,
        })
    }
}

impl TryFrom<&NormalizedSelection> for Selection {
    type Error = FederationError;

    fn try_from(val: &NormalizedSelection) -> Result<Self, Self::Error> {
        Ok(match val {
            NormalizedSelection::Field(normalized_field_selection) => {
                Selection::Field(Node::new(normalized_field_selection.deref().try_into()?))
            }
            NormalizedSelection::FragmentSpread(normalized_fragment_spread_selection) => {
                Selection::FragmentSpread(Node::new(
                    normalized_fragment_spread_selection.deref().into(),
                ))
            }
            NormalizedSelection::InlineFragment(normalized_inline_fragment_selection) => {
                Selection::InlineFragment(Node::new(
                    normalized_inline_fragment_selection.deref().try_into()?,
                ))
            }
        })
    }
}

impl TryFrom<&NormalizedFieldSelection> for Field {
    type Error = FederationError;

    fn try_from(val: &NormalizedFieldSelection) -> Result<Self, Self::Error> {
        let normalized_field = &val.field;
        let definition = normalized_field
            .data()
            .field_position
            .get(normalized_field.data().schema.schema())?
            .node
            .to_owned();
        let selection_set = if let Some(selection_set) = &val.selection_set {
            selection_set.try_into()?
        } else {
            SelectionSet {
                ty: definition.ty.inner_named_type().clone(),
                selections: vec![],
            }
        };
        Ok(Self {
            definition,
            alias: normalized_field.data().alias.to_owned(),
            name: normalized_field.data().name().to_owned(),
            arguments: normalized_field.data().arguments.deref().to_owned(),
            directives: normalized_field.data().directives.deref().to_owned(),
            selection_set,
        })
    }
}

impl TryFrom<&NormalizedInlineFragmentSelection> for InlineFragment {
    type Error = FederationError;

    fn try_from(val: &NormalizedInlineFragmentSelection) -> Result<Self, Self::Error> {
        let normalized_inline_fragment = &val.inline_fragment;
        Ok(Self {
            type_condition: normalized_inline_fragment
                .data()
                .type_condition_position
                .as_ref()
                .map(|pos| pos.type_name().clone()),
            directives: normalized_inline_fragment
                .data()
                .directives
                .deref()
                .to_owned(),
            selection_set: (&val.selection_set).try_into()?,
        })
    }
}

impl From<&NormalizedFragmentSpreadSelection> for FragmentSpread {
    fn from(val: &NormalizedFragmentSpreadSelection) -> Self {
        Self {
            fragment_name: val.data().fragment_name.to_owned(),
            directives: val.data().directives.deref().to_owned(),
        }
    }
}

impl Display for NormalizedOperation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let operation: Operation = match self.try_into() {
            Ok(operation) => operation,
            Err(_) => return Err(std::fmt::Error),
        };
        operation.serialize().fmt(f)
    }
}

impl Display for NormalizedSelectionSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let selection_set: SelectionSet = match self.try_into() {
            Ok(selection_set) => selection_set,
            Err(_) => return Err(std::fmt::Error),
        };
        selection_set.serialize().no_indent().fmt(f)
    }
}

impl Display for NormalizedSelection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let selection: Selection = match self.try_into() {
            Ok(selection) => selection,
            Err(_) => return Err(std::fmt::Error),
        };
        selection.serialize().no_indent().fmt(f)
    }
}

impl Display for NormalizedFieldSelection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let field: Field = match self.try_into() {
            Ok(field) => field,
            Err(_) => return Err(std::fmt::Error),
        };
        field.serialize().no_indent().fmt(f)
    }
}

impl Display for NormalizedInlineFragmentSelection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let inline_fragment: InlineFragment = match self.try_into() {
            Ok(inline_fragment) => inline_fragment,
            Err(_) => return Err(std::fmt::Error),
        };
        inline_fragment.serialize().no_indent().fmt(f)
    }
}

impl Display for NormalizedFragmentSpreadSelection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let fragment_spread: FragmentSpread = self.into();
        fragment_spread.serialize().no_indent().fmt(f)
    }
}

impl Display for NormalizedField {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // We create a selection with an empty selection set here, relying on `apollo-rs` to skip
        // serializing it when empty. Note we're implicitly relying on the lack of type-checking
        // in both `NormalizedFieldSelection` and `Field` display logic (specifically, we rely on
        // them not checking whether it is valid for the selection set to be empty).
        let selection = NormalizedFieldSelection {
            field: self.clone(),
            selection_set: None,
            sibling_typename: None,
        };
        selection.fmt(f)
    }
}

impl Display for NormalizedInlineFragment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // We can't use the same trick we did with `NormalizedField`'s display logic, since
        // selection sets are non-optional for inline fragment selections.
        let data = self.data();
        if let Some(type_name) = &data.type_condition_position {
            f.write_str("... on ")?;
            f.write_str(type_name.type_name())?;
        } else {
            f.write_str("...")?;
        }
        data.directives.serialize().no_indent().fmt(f)
    }
}

fn directives_with_sorted_arguments(directives: &DirectiveList) -> DirectiveList {
    let mut directives = directives.clone();
    for directive in &mut directives {
        directive
            .make_mut()
            .arguments
            .sort_by(|a1, a2| a1.name.cmp(&a2.name))
    }
    directives
}

fn is_deferred_selection(directives: &DirectiveList) -> bool {
    directives.has("defer")
}

/// Normalizes the selection set of the specified operation.
///
/// This method applies the following transformations:
/// - Merge selections with the same normalization "key".
/// - Expand fragment spreads into inline fragments.
/// - Remove `__schema` or `__type` introspection fields at all levels, as these shouldn't be
///   handled by query planning.
/// - Hoist fragment spreads/inline fragments into their parents if they have no directives and
///   their parent type matches.
pub(crate) fn normalize_operation(
    operation: &Operation,
    fragments: &IndexMap<Name, Node<Fragment>>,
    schema: &ValidFederationSchema,
    interface_types_with_interface_objects: &IndexSet<InterfaceTypeDefinitionPosition>,
) -> Result<NormalizedOperation, FederationError> {
    let mut normalized_selection_set = NormalizedSelectionSet::normalize_and_expand_fragments(
        &operation.selection_set,
        fragments,
        schema,
        &FragmentSpreadNormalizationOption::InlineFragmentSpread,
    )?;
    normalized_selection_set.optimize_sibling_typenames(interface_types_with_interface_objects)?;

    let normalized_fragments: HashMap<Name, Node<NormalizedFragment>> = fragments
        .iter()
        .map(|(name, fragment)| {
            (
                name.clone(),
                Node::new(NormalizedFragment::normalize(fragment, schema).unwrap()),
            )
        })
        .collect();

    let schema_definition_root_kind = match operation.operation_type {
        OperationType::Query => SchemaRootDefinitionKind::Query,
        OperationType::Mutation => SchemaRootDefinitionKind::Mutation,
        OperationType::Subscription => SchemaRootDefinitionKind::Subscription,
    };
    let normalized_operation = NormalizedOperation {
        schema: schema.clone(),
        root_kind: schema_definition_root_kind,
        name: operation.name.clone(),
        variables: Arc::new(operation.variables.clone()),
        directives: Arc::new(operation.directives.clone()),
        selection_set: normalized_selection_set,
        fragments: Arc::new(normalized_fragments),
    };
    Ok(normalized_operation)
}

fn is_interface_object(obj: &ObjectTypeDefinitionPosition, schema: &ValidFederationSchema) -> bool {
    if let Ok(intf_obj_directive) = get_federation_spec_definition_from_subgraph(schema)
        .and_then(|spec| spec.interface_object_directive(schema))
    {
        obj.try_get(&schema.schema()).is_some_and(|o| {
            o.directives
                .iter()
                .any(|d| d.name == intf_obj_directive.name)
        })
    } else {
        false
    }
}

fn runtime_types_intersect(
    type1: &CompositeTypeDefinitionPosition,
    type2: &CompositeTypeDefinitionPosition,
    schema: &ValidFederationSchema,
) -> bool {
    if type1 == type2 {
        return true;
    }

    if let (Ok(runtimes_1), Ok(runtimes_2)) = (
        schema.possible_runtime_types(type1.clone()),
        schema.possible_runtime_types(type2.clone()),
    ) {
        return runtimes_1
            .iter()
            .any(|r1| runtimes_2.iter().any(|r2| r1.type_name == r2.type_name));
    }

    false
}

fn print_possible_runtimes(
    composite_type: &CompositeTypeDefinitionPosition,
    schema: &ValidFederationSchema,
) -> String {
    schema
        .possible_runtime_types(composite_type.clone())
        .map_or_else(
            |_| "undefined".to_string(),
            |runtimes| {
                runtimes
                    .iter()
                    .map(|r| r.type_name.to_string())
                    .collect::<Vec<String>>()
                    .join(", ")
            },
        )
}

#[cfg(test)]
mod tests {
    use crate::query_plan::operation::normalize_operation;
    use crate::schema::position::InterfaceTypeDefinitionPosition;
    use crate::schema::ValidFederationSchema;
    use apollo_compiler::{name, ExecutableDocument};
    use indexmap::IndexSet;

    fn parse_schema_and_operation(
        schema_and_operation: &str,
    ) -> (ValidFederationSchema, ExecutableDocument) {
        let (schema, executable_document) =
            apollo_compiler::parse_mixed_validate(schema_and_operation, "document.graphql")
                .unwrap();
        let executable_document = executable_document.into_inner();
        let schema = ValidFederationSchema::new(schema).unwrap();
        (schema, executable_document)
    }

    #[test]
    fn expands_named_fragments() {
        let operation_with_named_fragment = r#"
query NamedFragmentQuery {
  foo {
    id
    ...Bar
  }
}

fragment Bar on Foo {
  bar
  baz
}

type Query {
  foo: Foo
}

type Foo {
  id: ID!
  bar: String!
  baz: Int
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_with_named_fragment);
        if let Some(operation) = executable_document
            .named_operations
            .get_mut("NamedFragmentQuery")
        {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();

            let expected = r#"query NamedFragmentQuery {
  foo {
    id
    bar
    baz
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn expands_and_deduplicates_fragments() {
        let operation_with_named_fragment = r#"
query NestedFragmentQuery {
  foo {
    ...FirstFragment
    ...SecondFragment
  }
}

fragment FirstFragment on Foo {
  id
  bar
  baz
}

fragment SecondFragment on Foo {
  id
  bar
}

type Query {
  foo: Foo
}

type Foo {
  id: ID!
  bar: String!
  baz: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_with_named_fragment);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();

            let expected = r#"query NestedFragmentQuery {
  foo {
    id
    bar
    baz
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn can_remove_introspection_selections() {
        let operation_with_introspection = r#"
query TestIntrospectionQuery {
  __schema {
    types {
      name
    }
  }
}

type Query {
  foo: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_with_introspection);
        if let Some(operation) = executable_document
            .named_operations
            .get_mut("TestIntrospectionQuery")
        {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();

            assert!(normalized_operation.selection_set.selections.is_empty());
        }
    }

    #[test]
    fn merge_same_fields_without_directives() {
        let operation_string = r#"
query Test {
  t {
    v1
  }
  t {
    v2
 }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) = parse_schema_and_operation(operation_string);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test {
  t {
    v1
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn merge_same_fields_with_same_directive() {
        let operation_with_directives = r#"
query Test($skipIf: Boolean!) {
  t @skip(if: $skipIf) {
    v1
  }
  t @skip(if: $skipIf) {
    v2
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_with_directives);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test($skipIf: Boolean!) {
  t @skip(if: $skipIf) {
    v1
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn merge_same_fields_with_same_directive_but_different_arg_order() {
        let operation_with_directives_different_arg_order = r#"
query Test($skipIf: Boolean!) {
  t @customSkip(if: $skipIf, label: "foo") {
    v1
  }
  t @customSkip(label: "foo", if: $skipIf) {
    v2
  }
}

directive @customSkip(if: Boolean!, label: String!) on FIELD | INLINE_FRAGMENT

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_with_directives_different_arg_order);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test($skipIf: Boolean!) {
  t @customSkip(if: $skipIf, label: "foo") {
    v1
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn do_not_merge_when_only_one_field_specifies_directive() {
        let operation_one_field_with_directives = r#"
query Test($skipIf: Boolean!) {
  t {
    v1
  }
  t @skip(if: $skipIf) {
    v2
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_one_field_with_directives);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test($skipIf: Boolean!) {
  t {
    v1
  }
  t @skip(if: $skipIf) {
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn do_not_merge_when_fields_have_different_directives() {
        let operation_different_directives = r#"
query Test($skip1: Boolean!, $skip2: Boolean!) {
  t @skip(if: $skip1) {
    v1
  }
  t @skip(if: $skip2) {
    v2
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_different_directives);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test($skip1: Boolean!, $skip2: Boolean!) {
  t @skip(if: $skip1) {
    v1
  }
  t @skip(if: $skip2) {
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    // TODO enable when @defer is available in apollo-rs
    #[ignore]
    #[test]
    fn do_not_merge_fields_with_defer_directive() {
        let operation_defer_fields = r#"
query Test {
  t @defer {
    v1
  }
  t @defer {
    v2
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) = parse_schema_and_operation(operation_defer_fields);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test {
  t @defer {
    v1
  }
  t @defer {
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    // TODO enable when @defer is available in apollo-rs
    #[ignore]
    #[test]
    fn merge_nested_field_selections() {
        let nested_operation = r#"
query Test {
  t {
    t1
    v @defer {
      v1
    }
  }
  t {
    t1
    t2
    v @defer {
      v2
    }
  }
}

type Query {
  t: T
}

type T {
  t1: Int
  t2: String
  v: V
}

type V {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) = parse_schema_and_operation(nested_operation);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test {
  t {
    t1
    v @defer {
      v1
    }
    t2
    v @defer {
      v2
    }
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    //
    // inline fragments
    //

    #[test]
    fn merge_same_fragment_without_directives() {
        let operation_with_fragments = r#"
query Test {
  t {
    ... on T {
      v1
    }
    ... on T {
      v2
    }
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_with_fragments);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test {
  t {
    v1
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn merge_same_fragments_with_same_directives() {
        let operation_fragments_with_directives = r#"
query Test($skipIf: Boolean!) {
  t {
    ... on T @skip(if: $skipIf) {
      v1
    }
    ... on T @skip(if: $skipIf) {
      v2
    }
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_fragments_with_directives);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test($skipIf: Boolean!) {
  t {
    ... on T @skip(if: $skipIf) {
      v1
      v2
    }
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn merge_same_fragments_with_same_directive_but_different_arg_order() {
        let operation_fragments_with_directives_args_order = r#"
query Test($skipIf: Boolean!) {
  t {
    ... on T @customSkip(if: $skipIf, label: "foo") {
      v1
    }
    ... on T @customSkip(label: "foo", if: $skipIf) {
      v2
    }
  }
}

directive @customSkip(if: Boolean!, label: String!) on FIELD | INLINE_FRAGMENT

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_fragments_with_directives_args_order);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test($skipIf: Boolean!) {
  t {
    ... on T @customSkip(if: $skipIf, label: "foo") {
      v1
      v2
    }
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn do_not_merge_when_only_one_fragment_specifies_directive() {
        let operation_one_fragment_with_directive = r#"
query Test($skipIf: Boolean!) {
  t {
    ... on T {
      v1
    }
    ... on T @skip(if: $skipIf) {
      v2
    }
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_one_fragment_with_directive);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test($skipIf: Boolean!) {
  t {
    v1
    ... on T @skip(if: $skipIf) {
      v2
    }
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn do_not_merge_when_fragments_have_different_directives() {
        let operation_fragments_with_different_directive = r#"
query Test($skip1: Boolean!, $skip2: Boolean!) {
  t {
    ... on T @skip(if: $skip1) {
      v1
    }
    ... on T @skip(if: $skip2) {
      v2
    }
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_fragments_with_different_directive);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test($skip1: Boolean!, $skip2: Boolean!) {
  t {
    ... on T @skip(if: $skip1) {
      v1
    }
    ... on T @skip(if: $skip2) {
      v2
    }
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    // TODO enable when @defer is available in apollo-rs
    #[ignore]
    #[test]
    fn do_not_merge_fragments_with_defer_directive() {
        let operation_fragments_with_defer = r#"
query Test {
  t {
    ... on T @defer {
      v1
    }
    ... on T @defer {
      v2
    }
  }
}

type Query {
  t: T
}

type T {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_fragments_with_defer);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test {
  t {
    ... on T @defer {
      v1
    }
    ... on T @defer {
      v2
    }
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    // TODO enable when @defer is available in apollo-rs
    #[ignore]
    #[test]
    fn merge_nested_fragments() {
        let operation_nested_fragments = r#"
query Test {
  t {
    ... on T {
      t1
    }
    ... on T {
      v @defer {
        v1
      }
    }
  }
  t {
    ... on T {
      t1
      t2
    }
    ... on T {
      v @defer {
        v2
      }
    }
  }
}

type Query {
  t: T
}

type T {
  t1: Int
  t2: String
  v: V
}

type V {
  v1: Int
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_nested_fragments);
        if let Some((_, operation)) = executable_document.named_operations.first_mut() {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query Test {
  t {
    t1
    v @defer {
      v1
    }
    t2
    v @defer {
      v2
    }
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        } else {
            panic!("unable to parse document")
        }
    }

    #[test]
    fn removes_sibling_typename() {
        let operation_with_typename = r#"
query TestQuery {
  foo {
    __typename
    v1
    v2
  }
}

type Query {
  foo: Foo
}

type Foo {
  v1: ID!
  v2: String
}
"#;
        let (schema, mut executable_document) = parse_schema_and_operation(operation_with_typename);
        if let Some(operation) = executable_document.named_operations.get_mut("TestQuery") {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query TestQuery {
  foo {
    v1
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn keeps_typename_if_no_other_selection() {
        let operation_with_single_typename = r#"
query TestQuery {
  foo {
    __typename
  }
}

type Query {
  foo: Foo
}

type Foo {
  v1: ID!
  v2: String
}
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_with_single_typename);
        if let Some(operation) = executable_document.named_operations.get_mut("TestQuery") {
            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &IndexSet::new(),
            )
            .unwrap();
            let expected = r#"query TestQuery {
  foo {
    __typename
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        }
    }

    #[test]
    fn keeps_typename_for_interface_object() {
        let operation_with_intf_object_typename = r#"
query TestQuery {
  foo {
    __typename
    v1
    v2
  }
}

directive @interfaceObject on OBJECT
directive @key(fields: FieldSet!, resolvable: Boolean = true) repeatable on OBJECT | INTERFACE

type Query {
  foo: Foo
}

type Foo @interfaceObject @key(fields: "id") {
  v1: ID!
  v2: String
}

scalar FieldSet
"#;
        let (schema, mut executable_document) =
            parse_schema_and_operation(operation_with_intf_object_typename);
        if let Some(operation) = executable_document.named_operations.get_mut("TestQuery") {
            let mut interface_objects: IndexSet<InterfaceTypeDefinitionPosition> = IndexSet::new();
            interface_objects.insert(InterfaceTypeDefinitionPosition {
                type_name: name!("Foo"),
            });

            let normalized_operation = normalize_operation(
                operation,
                &executable_document.fragments,
                &schema,
                &interface_objects,
            )
            .unwrap();
            let expected = r#"query TestQuery {
  foo {
    __typename
    v1
    v2
  }
}"#;
            let actual = normalized_operation.to_string();
            assert_eq!(expected, actual);
        }
    }
}
