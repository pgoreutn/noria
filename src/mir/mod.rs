use nom_sql::{Column, ColumnConstraint, ColumnSpecification, Operator, OrderType};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Error, Formatter};
use std::rc::Rc;

use flow::Migration;
use flow::core::DataType;
use flow::prelude::NodeIndex;
use ops;
use ops::grouped::aggregate::Aggregation as AggregationKind;
use ops::grouped::extremum::Extremum as ExtremumKind;
use ops::join::{Join, JoinType};
use ops::latest::Latest;
use ops::project::Project;
use sql::QueryFlowParts;

pub mod reuse;
mod rewrite;
mod optimize;

#[derive(Clone, Debug)]
pub enum FlowNode {
    New(NodeIndex),
    Existing(NodeIndex),
}

impl FlowNode {
    pub fn address(&self) -> NodeIndex {
        match *self {
            FlowNode::New(na) | FlowNode::Existing(na) => na,
        }
    }
}

/// Helper enum to avoid having separate `make_aggregation_node` and `make_extremum_node` functions
pub enum GroupedNodeType {
    Aggregation(ops::grouped::aggregate::Aggregation),
    Extremum(ops::grouped::extremum::Extremum),
    GroupConcat(String),
}

pub type MirNodeRef = Rc<RefCell<MirNode>>;

#[derive(Clone, Debug)]
pub struct MirQuery {
    pub name: String,
    pub roots: Vec<MirNodeRef>,
    pub leaf: MirNodeRef,
}

impl MirQuery {
    pub fn singleton(name: &str, node: MirNodeRef) -> MirQuery {
        MirQuery {
            name: String::from(name),
            roots: vec![node.clone()],
            leaf: node,
        }
    }

    #[cfg(test)]
    pub fn topo_nodes(&self) -> Vec<MirNodeRef> {
        use std::collections::VecDeque;

        let mut nodes = Vec::new();

        // starting at the roots, traverse in topological order
        let mut node_queue: VecDeque<_> = self.roots.iter().cloned().collect();
        let mut in_edge_counts = HashMap::new();
        for n in &node_queue {
            in_edge_counts.insert(n.borrow().versioned_name(), 0);
        }
        while let Some(n) = node_queue.pop_front() {
            assert_eq!(in_edge_counts[&n.borrow().versioned_name()], 0);

            nodes.push(n.clone());

            for child in n.borrow().children.iter() {
                let nd = child.borrow().versioned_name();
                let in_edges = if in_edge_counts.contains_key(&nd) {
                    in_edge_counts[&nd]
                } else {
                    child.borrow().ancestors.len()
                };
                assert!(in_edges >= 1, format!("{} has no incoming edges!", nd));
                if in_edges == 1 {
                    // last edge removed
                    node_queue.push_back(child.clone());
                }
                in_edge_counts.insert(nd, in_edges - 1);
            }
        }
        nodes
    }

    pub fn into_flow_parts(&mut self, mig: &mut Migration) -> QueryFlowParts {
        use std::collections::VecDeque;

        let mut new_nodes = Vec::new();
        let mut reused_nodes = Vec::new();

        // starting at the roots, add nodes in topological order
        let mut node_queue = VecDeque::new();
        node_queue.extend(self.roots.iter().cloned());
        let mut in_edge_counts = HashMap::new();
        for n in &node_queue {
            in_edge_counts.insert(n.borrow().versioned_name(), 0);
        }
        while !node_queue.is_empty() {
            let n = node_queue.pop_front().unwrap();
            assert_eq!(in_edge_counts[&n.borrow().versioned_name()], 0);

            let flow_node = n.borrow_mut().into_flow_parts(mig);
            match flow_node {
                FlowNode::New(na) => new_nodes.push(na),
                FlowNode::Existing(na) => reused_nodes.push(na),
            }

            for child in n.borrow().children.iter() {
                let nd = child.borrow().versioned_name();
                let in_edges = if in_edge_counts.contains_key(&nd) {
                    in_edge_counts[&nd]
                } else {
                    child.borrow().ancestors.len()
                };
                assert!(in_edges >= 1);
                if in_edges == 1 {
                    // last edge removed
                    node_queue.push_back(child.clone());
                }
                in_edge_counts.insert(nd, in_edges - 1);
            }
        }

        let leaf_na = self.leaf
            .borrow()
            .flow_node
            .as_ref()
            .expect("Leaf must have FlowNode by now")
            .address();

        QueryFlowParts {
            name: self.name.clone(),
            new_nodes: new_nodes,
            reused_nodes: reused_nodes,
            query_leaf: leaf_na,
        }
    }

    pub fn optimize(mut self) -> MirQuery {
        rewrite::pull_required_base_columns(&mut self);
        optimize::optimize(self)
    }

    pub fn optimize_post_reuse(mut self) -> MirQuery {
        optimize::optimize_post_reuse(&mut self);
        self
    }
}

impl Display for MirQuery {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        use std::collections::VecDeque;

        // starting at the roots, print nodes in topological order
        let mut node_queue = VecDeque::new();
        node_queue.extend(self.roots.iter().cloned());
        let mut in_edge_counts = HashMap::new();
        for n in &node_queue {
            in_edge_counts.insert(n.borrow().versioned_name(), 0);
        }

        writeln!(f, "digraph {{")?;
        writeln!(f, "node [shape=record, fontsize=10]")?;

        while !node_queue.is_empty() {
            let n = node_queue.pop_front().unwrap();
            assert_eq!(in_edge_counts[&n.borrow().versioned_name()], 0);

            let vn = n.borrow().versioned_name();
            writeln!(
                f,
                "\"{}\" [label=\"{{ {} | {} }}\"]",
                vn,
                vn,
                n.borrow()
                    .columns()
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", \\n")
            )?;

            for child in n.borrow().children.iter() {
                let nd = child.borrow().versioned_name();
                writeln!(f, "\"{}\" -> \"{}\"", n.borrow().versioned_name(), nd)?;
                let in_edges = if in_edge_counts.contains_key(&nd) {
                    in_edge_counts[&nd]
                } else {
                    child.borrow().ancestors.len()
                };
                assert!(in_edges >= 1);
                if in_edges == 1 {
                    // last edge removed
                    node_queue.push_back(child.clone());
                }
                in_edge_counts.insert(nd, in_edges - 1);
            }
        }
        write!(f, "}}")?;

        Ok(())
    }
}

pub struct MirNode {
    name: String,
    from_version: usize,
    columns: Vec<Column>,
    pub(crate) inner: MirNodeType,
    ancestors: Vec<MirNodeRef>,
    children: Vec<MirNodeRef>,
    pub flow_node: Option<FlowNode>,
}

impl MirNode {
    pub fn new(
        name: &str,
        v: usize,
        columns: Vec<Column>,
        inner: MirNodeType,
        ancestors: Vec<MirNodeRef>,
        children: Vec<MirNodeRef>,
    ) -> MirNodeRef {
        let mn = MirNode {
            name: String::from(name),
            from_version: v,
            columns: columns,
            inner: inner,
            ancestors: ancestors.clone(),
            children: children.clone(),
            flow_node: None,
        };

        let rc_mn = Rc::new(RefCell::new(mn));

        // register as child on ancestors
        for ref ancestor in ancestors {
            ancestor.borrow_mut().add_child(rc_mn.clone());
        }

        rc_mn
    }

    /// Adapts an existing `Base`-type MIR Node with the specified column additions and removals.
    pub fn adapt_base(
        node: MirNodeRef,
        added_cols: Vec<&ColumnSpecification>,
        removed_cols: Vec<&ColumnSpecification>,
    ) -> MirNodeRef {
        let over_node = node.borrow();
        match over_node.inner {
            MirNodeType::Base {
                ref column_specs,
                ref keys,
                transactional,
                ..
            } => {
                let new_column_specs: Vec<(ColumnSpecification, Option<usize>)> = column_specs
                    .into_iter()
                    .cloned()
                    .filter(|&(ref cs, _)| !removed_cols.contains(&cs))
                    .chain(
                        added_cols
                            .iter()
                            .map(|c| ((*c).clone(), None))
                            .collect::<Vec<(ColumnSpecification, Option<usize>)>>(),
                    )
                    .collect();
                let new_columns: Vec<Column> = new_column_specs
                    .iter()
                    .map(|&(ref cs, _)| cs.column.clone())
                    .collect();

                assert_eq!(
                    new_column_specs.len(),
                    over_node.columns.len() + added_cols.len() - removed_cols.len()
                );

                let new_inner = MirNodeType::Base {
                    column_specs: new_column_specs,
                    keys: keys.clone(),
                    transactional: transactional,
                    adapted_over: Some(BaseNodeAdaptation {
                        over: node.clone(),
                        columns_added: added_cols.into_iter().cloned().collect(),
                        columns_removed: removed_cols.into_iter().cloned().collect(),
                    }),
                };
                return MirNode::new(
                    &over_node.name,
                    over_node.from_version,
                    new_columns,
                    new_inner,
                    vec![],
                    over_node.children.clone(),
                );
            }
            _ => unreachable!(),
        }
    }

    /// Wraps an existing MIR node into a `Reuse` node.
    /// Note that this does *not* wire the reuse node into ancestors or children of the original
    /// node; if required, this is the responsibility of the caller.
    pub fn reuse(node: MirNodeRef, v: usize) -> MirNodeRef {
        let rcn = node.clone();

        let mn = MirNode {
            name: node.borrow().name.clone(),
            from_version: v,
            columns: node.borrow().columns.clone(),
            inner: MirNodeType::Reuse { node: rcn },
            ancestors: vec![],
            children: vec![],
            flow_node: None, // will be set in `into_flow_parts`
        };

        let rc_mn = Rc::new(RefCell::new(mn));

        rc_mn
    }

    pub fn can_reuse_as(&self, for_node: &MirNode) -> bool {
        let mut have_all_columns = true;
        for c in &for_node.columns {
            if !self.columns.contains(c) {
                have_all_columns = false;
                break;
            }
        }

        have_all_columns && self.inner.can_reuse_as(&for_node.inner)
    }

    // currently unused
    #[allow(dead_code)]
    pub fn add_ancestor(&mut self, a: MirNodeRef) {
        self.ancestors.push(a)
    }

    pub fn remove_ancestor(&mut self, a: MirNodeRef) {
        match self.ancestors.iter().position(|x| {
            x.borrow().versioned_name() == a.borrow().versioned_name()
        }) {
            None => (),
            Some(idx) => {
                self.ancestors.remove(idx);
            }
        }
    }

    pub fn add_child(&mut self, c: MirNodeRef) {
        self.children.push(c)
    }

    pub fn remove_child(&mut self, a: MirNodeRef) {
        match self.children.iter().position(|x| {
            x.borrow().versioned_name() == a.borrow().versioned_name()
        }) {
            None => (),
            Some(idx) => {
                self.children.remove(idx);
            }
        }
    }

    pub fn add_column(&mut self, c: Column) {
        self.columns.push(c.clone());
        self.inner.add_column(c);
    }

    pub fn ancestors(&self) -> &[MirNodeRef] {
        self.ancestors.as_slice()
    }

    pub fn children(&self) -> &[MirNodeRef] {
        self.children.as_slice()
    }

    pub fn columns(&self) -> &[Column] {
        self.columns.as_slice()
    }

    pub fn column_id_for_column(&self, c: &Column) -> usize {
        match self.inner {
            // if we're a base, translate to absolute column ID (taking into account deleted
            // columns). We use the column specifications here, which track a tuple of (column
            // spec, absolute column ID).
            // Note that `rposition` is required because multiple columns of the same name might
            // exist if a column has been removed and re-added. We always use the latest column,
            // and assume that only one column of the same name ever exists at the same time.
            MirNodeType::Base {
                ref column_specs, ..
            } => match column_specs.iter().rposition(|cs| cs.0.column == *c) {
                None => panic!("tried to look up non-existent column {:?}", c),
                Some(id) => column_specs[id]
                    .1
                    .expect("must have an absolute column ID on base"),
            },
            MirNodeType::Reuse { ref node } => node.borrow().column_id_for_column(c),
            // otherwise, just look up in the column set
            _ => match self.columns.iter().position(|cc| cc == c) {
                None => panic!("tried to look up non-existent column {:?}", c.name),
                Some(id) => id,
            },
        }
    }

    pub fn column_specifications(&self) -> &[(ColumnSpecification, Option<usize>)] {
        match self.inner {
            MirNodeType::Base {
                ref column_specs, ..
            } => column_specs.as_slice(),
            _ => panic!("non-base MIR nodes don't have column specifications!"),
        }
    }

    fn flow_node_addr(&self) -> Result<NodeIndex, String> {
        match self.flow_node {
            Some(FlowNode::New(na)) | Some(FlowNode::Existing(na)) => Ok(na),
            None => Err(format!(
                "MIR node \"{}\" does not have an associated FlowNode",
                self.versioned_name()
            )),
        }
    }

    #[allow(dead_code)]
    pub fn is_reused(&self) -> bool {
        match self.inner {
            MirNodeType::Reuse { .. } => true,
            _ => false,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn referenced_columns(&self) -> Vec<Column> {
        // all projected columns, minus those with aliases, which we add with their original names
        // below. This is important because we'll otherwise end up searching (and fail to find)
        // aliased columns further up in the MIR graph.
        let mut columns: Vec<Column> = self.columns
            .iter()
            .filter(|c| c.alias.is_none() || c.function.is_some()) // alias ok if computed column
            .cloned()
            .collect();

        // + any parent columns referenced internally by the operator
        match self.inner {
            MirNodeType::Aggregation { ref on, .. } |
            MirNodeType::Extremum { ref on, .. } |
            MirNodeType::GroupConcat { ref on, .. } => {
                // need the "over" column
                if !columns.contains(on) {
                    columns.push(on.clone());
                }
            }
            MirNodeType::Filter { .. } => {
                let parent = self.ancestors.iter().next().unwrap();
                // need all parent columns
                for c in parent.borrow().columns() {
                    if !columns.contains(&c) {
                        columns.push(c.clone());
                    }
                }
            }
            MirNodeType::Project { ref emit, .. } => for c in emit {
                if !columns.contains(&c) {
                    columns.push(c.clone());
                }
            },
            _ => (),
        }
        columns
    }

    pub fn versioned_name(&self) -> String {
        format!("{}_v{}", self.name, self.from_version)
    }

    /// Produce a compact, human-readable description of this node; analogous to the method of the
    /// same name on `Ingredient`.
    fn description(&self) -> String {
        format!(
            "{}: {} / {} columns",
            self.versioned_name(),
            self.inner.description(),
            self.columns.len()
        )
    }

    fn into_flow_parts(&mut self, mig: &mut Migration) -> FlowNode {
        let name = self.name.clone();
        match self.flow_node {
            None => {
                let flow_node = match self.inner {
                    MirNodeType::Aggregation {
                        ref on,
                        ref group_by,
                        ref kind,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_grouped_node(
                            &name,
                            parent,
                            self.columns.as_slice(),
                            on,
                            group_by,
                            GroupedNodeType::Aggregation(kind.clone()),
                            mig,
                        )
                    }
                    MirNodeType::Base {
                        ref mut column_specs,
                        ref keys,
                        transactional,
                        ref adapted_over,
                    } => match *adapted_over {
                        None => make_base_node(
                            &name,
                            column_specs.as_mut_slice(),
                            keys,
                            mig,
                            transactional,
                        ),
                        Some(ref bna) => adapt_base_node(
                            bna.over.clone(),
                            mig,
                            column_specs.as_mut_slice(),
                            &bna.columns_added,
                            &bna.columns_removed,
                        ),
                    },
                    MirNodeType::Extremum {
                        ref on,
                        ref group_by,
                        ref kind,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_grouped_node(
                            &name,
                            parent,
                            self.columns.as_slice(),
                            on,
                            group_by,
                            GroupedNodeType::Extremum(kind.clone()),
                            mig,
                        )
                    }
                    MirNodeType::Filter { ref conditions } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_filter_node(&name, parent, self.columns.as_slice(), conditions, mig)
                    }
                    MirNodeType::GroupConcat {
                        ref on,
                        ref separator,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        let group_cols = parent.borrow().columns().iter().cloned().collect();
                        make_grouped_node(
                            &name,
                            parent,
                            self.columns.as_slice(),
                            on,
                            &group_cols,
                            GroupedNodeType::GroupConcat(separator.to_string()),
                            mig,
                        )
                    }
                    MirNodeType::Identity => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_identity_node(&name, parent, self.columns.as_slice(), mig)
                    }
                    MirNodeType::Join {
                        ref on_left,
                        ref on_right,
                        ref project,
                    } => {
                        assert_eq!(self.ancestors.len(), 2);
                        let left = self.ancestors[0].clone();
                        let right = self.ancestors[1].clone();
                        make_join_node(
                            &name,
                            left,
                            right,
                            self.columns.as_slice(),
                            on_left,
                            on_right,
                            project,
                            JoinType::Inner,
                            mig,
                        )
                    }
                    MirNodeType::Latest { ref group_by } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_latest_node(&name, parent, self.columns.as_slice(), group_by, mig)
                    }
                    MirNodeType::Leaf { ref keys, .. } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        materialize_leaf_node(&parent, keys, mig);
                        // TODO(malte): below is yucky, but required to satisfy the type system:
                        // each match arm must return a `FlowNode`, so we use the parent's one
                        // here.
                        let node = match *parent.borrow().flow_node.as_ref().unwrap() {
                            FlowNode::New(na) => FlowNode::Existing(na),
                            ref n @ FlowNode::Existing(..) => n.clone(),
                        };
                        node
                    }
                    MirNodeType::LeftJoin {
                        ref on_left,
                        ref on_right,
                        ref project,
                    } => {
                        assert_eq!(self.ancestors.len(), 2);
                        let left = self.ancestors[0].clone();
                        let right = self.ancestors[1].clone();
                        make_join_node(
                            &name,
                            left,
                            right,
                            self.columns.as_slice(),
                            on_left,
                            on_right,
                            project,
                            JoinType::Left,
                            mig,
                        )
                    }
                    MirNodeType::Project {
                        ref emit,
                        ref literals,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_project_node(
                            &name,
                            parent,
                            self.columns.as_slice(),
                            emit,
                            literals,
                            mig,
                        )
                    }
                    MirNodeType::Reuse { ref node } => {
                        match *node.borrow()
                           .flow_node
                           .as_ref()
                           .expect("Reused MirNode must have FlowNode") {
                               // "New" => flow node was originally created for the node that we
                               // are reusing
                               FlowNode::New(na) |
                               // "Existing" => flow node was already reused from some other
                               // MIR node
                               FlowNode::Existing(na) => FlowNode::Existing(na),
                        }
                    }
                    MirNodeType::Union { ref emit } => {
                        assert_eq!(self.ancestors.len(), emit.len());
                        make_union_node(&name, self.columns.as_slice(), emit, self.ancestors(), mig)
                    }
                    MirNodeType::TopK {
                        ref order,
                        ref group_by,
                        ref k,
                        ref offset,
                    } => {
                        assert_eq!(self.ancestors.len(), 1);
                        let parent = self.ancestors[0].clone();
                        make_topk_node(
                            &name,
                            parent,
                            self.columns.as_slice(),
                            order,
                            group_by,
                            *k,
                            *offset,
                            mig,
                        )
                    }
                };

                // any new flow nodes have been instantiated by now, so we replace them with
                // existing ones, but still return `FlowNode::New` below in order to notify higher
                // layers of the new nodes.
                self.flow_node = match flow_node {
                    FlowNode::New(na) => Some(FlowNode::Existing(na)),
                    ref n @ FlowNode::Existing(..) => Some(n.clone()),
                };
                flow_node
            }
            Some(ref flow_node) => flow_node.clone(),
        }
    }
}

/// Specifies the adapatation of an existing base node by column addition/removal.
/// `over` is a `MirNode` of type `Base`.
pub struct BaseNodeAdaptation {
    over: MirNodeRef,
    columns_added: Vec<ColumnSpecification>,
    columns_removed: Vec<ColumnSpecification>,
}

pub enum MirNodeType {
    /// over column, group_by columns
    Aggregation {
        on: Column,
        group_by: Vec<Column>,
        kind: AggregationKind,
    },
    /// column specifications, keys (non-compound), tx flag, adapted base
    Base {
        column_specs: Vec<(ColumnSpecification, Option<usize>)>,
        keys: Vec<Column>,
        transactional: bool,
        adapted_over: Option<BaseNodeAdaptation>,
    },
    /// over column, group_by columns
    Extremum {
        on: Column,
        group_by: Vec<Column>,
        kind: ExtremumKind,
    },
    /// filter conditions (one for each parent column)
    Filter {
        conditions: Vec<Option<(Operator, DataType)>>,
    },
    /// over column, separator
    GroupConcat { on: Column, separator: String },
    /// no extra info required
    Identity,
    /// left node, right node, on left columns, on right columns, emit columns
    Join {
        on_left: Vec<Column>,
        on_right: Vec<Column>,
        project: Vec<Column>,
    },
    /// on left column, on right column, emit columns
    // currently unused
    #[allow(dead_code)]
    LeftJoin {
        on_left: Vec<Column>,
        on_right: Vec<Column>,
        project: Vec<Column>,
    },
    /// group columns
    // currently unused
    #[allow(dead_code)]
    Latest { group_by: Vec<Column> },
    /// emit columns
    Project {
        emit: Vec<Column>,
        literals: Vec<(String, DataType)>,
    },
    /// emit columns
    Union { emit: Vec<Vec<Column>> },
    /// order function, group columns, k
    TopK {
        order: Option<Vec<(Column, OrderType)>>,
        group_by: Vec<Column>,
        k: usize,
        offset: usize,
    },
    /// reuse another node
    Reuse { node: MirNodeRef },
    /// leaf (reader) node, keys
    Leaf { node: MirNodeRef, keys: Vec<Column> },
}

impl MirNodeType {
    fn description(&self) -> String {
        format!("{:?}", self)
    }

    fn add_column(&mut self, c: Column) {
        match *self {
            MirNodeType::Aggregation {
                ref mut group_by, ..
            } => {
                group_by.push(c);
            }
            MirNodeType::Base { .. } => panic!("can't add columns to base nodes!"),
            MirNodeType::Extremum {
                ref mut group_by, ..
            } => {
                group_by.push(c);
            }
            MirNodeType::Filter { ref mut conditions } => {
                conditions.push(None);
            }
            MirNodeType::Join {
                ref mut project, ..
            } |
            MirNodeType::LeftJoin {
                ref mut project, ..
            } => {
                project.push(c);
            }
            MirNodeType::Project { ref mut emit, .. } => {
                emit.push(c);
            }
            MirNodeType::Union { .. } => unimplemented!(),
            MirNodeType::TopK {
                ref mut group_by, ..
            } => {
                group_by.push(c);
            }
            _ => (),
        }
    }

    fn can_reuse_as(&self, other: &MirNodeType) -> bool {
        match *self {
            MirNodeType::Reuse { .. } => (), // handled below
            _ => {
                // we're not a `Reuse` ourselves, but the other side might be
                match *other {
                    // it is, so dig deeper
                    MirNodeType::Reuse { ref node } => {
                        // this does not check the projected columns of the inner node for two
                        // reasons:
                        // 1) our own projected columns aren't accessible on `MirNodeType`, but
                        //    only on the outer `MirNode`, which isn't accessible here; but more
                        //    importantly
                        // 2) since this is already a node reuse, the inner, reused node must have
                        //    *at least* a superset of our own (inaccessible) projected columns.
                        // Hence, it is sufficient to check the projected columns on the parent
                        // `MirNode`, and if that check passes, it also holds for the nodes reused
                        // here.
                        return self.can_reuse_as(&node.borrow().inner);
                    }
                    _ => (), // handled below
                }
            }
        }

        match *self {
            MirNodeType::Aggregation {
                on: ref our_on,
                group_by: ref our_group_by,
                kind: ref our_kind,
            } => {
                match *other {
                    MirNodeType::Aggregation {
                        ref on,
                        ref group_by,
                        ref kind,
                    } => {
                        // TODO(malte): this is stricter than it needs to be, as it could cover
                        // COUNT-as-SUM-style relationships.
                        our_on == on && our_group_by == group_by && our_kind == kind
                    }
                    _ => false,
                }
            }
            MirNodeType::Base {
                column_specs: ref our_column_specs,
                keys: ref our_keys,
                transactional: our_transactional,
                adapted_over: ref our_adapted_over,
            } => {
                match *other {
                    MirNodeType::Base {
                        ref column_specs,
                        ref keys,
                        transactional,
                        ..
                    } => {
                        assert_eq!(our_transactional, transactional);
                        // if we are instructed to adapt an earlier base node, we cannot reuse
                        // anything directly; we'll have to keep a new MIR node here.
                        if our_adapted_over.is_some() {
                            // TODO(malte): this is a bit more conservative than it needs to be,
                            // since base node adaptation actually *changes* the underlying base
                            // node, so we will actually reuse. However, returning false here
                            // terminates the reuse search unnecessarily. We should handle this
                            // special case.
                            return false;
                        }
                        // note that as long as we are not adapting a previous base node,
                        // we do *not* need `adapted_over` to *match*, since current reuse
                        // does not depend on how base node was created from an earlier one
                        our_column_specs == column_specs && our_keys == keys
                    }
                    _ => false,
                }
            }
            MirNodeType::Filter {
                conditions: ref our_conditions,
            } => match *other {
                MirNodeType::Filter { ref conditions } => our_conditions == conditions,
                _ => false,
            },
            MirNodeType::Join {
                on_left: ref our_on_left,
                on_right: ref our_on_right,
                project: ref our_project,
            } => {
                match *other {
                    MirNodeType::Join {
                        ref on_left,
                        ref on_right,
                        ref project,
                    } => {
                        // TODO(malte): column order does not actually need to match, but this only
                        // succeeds if it does.
                        our_on_left == on_left && our_on_right == on_right && our_project == project
                    }
                    _ => false,
                }
            }
            MirNodeType::Project {
                emit: ref our_emit,
                literals: ref our_literals,
            } => match *other {
                MirNodeType::Project {
                    ref emit,
                    ref literals,
                } => our_emit == emit && our_literals == literals,
                _ => false,
            },
            MirNodeType::Reuse { node: ref us } => {
                match *other {
                    // both nodes are `Reuse` nodes, so we simply compare the both sides' reuse
                    // target
                    MirNodeType::Reuse { ref node } => us.borrow().can_reuse_as(&*node.borrow()),
                    // we're a `Reuse`, the other side isn't, so see if our reuse target's `inner`
                    // can be reused for the other side. It's sufficient to check the target's
                    // `inner` because reuse implies that our target has at least a superset of our
                    // projected columns (see earlier comment).
                    _ => us.borrow().inner.can_reuse_as(other),
                }
            }
            MirNodeType::TopK {
                order: ref our_order,
                group_by: ref our_group_by,
                k: our_k,
                offset: our_offset,
            } => match *other {
                MirNodeType::TopK {
                    ref order,
                    ref group_by,
                    k,
                    offset,
                } => {
                    order == our_order && group_by == our_group_by && k == our_k &&
                        offset == our_offset
                }
                _ => false,
            },
            MirNodeType::Leaf {
                keys: ref our_keys, ..
            } => match *other {
                MirNodeType::Leaf { ref keys, .. } => keys == our_keys,
                _ => false,
            },
            _ => unimplemented!(),
        }
    }
}

impl Debug for MirNode {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(
            f,
            "{}, {} ancestors ({}), {} children ({})",
            self.description(),
            self.ancestors.len(),
            self.ancestors
                .iter()
                .map(|a| a.borrow().versioned_name())
                .collect::<Vec<_>>()
                .join(", "),
            self.children.len(),
            self.children
                .iter()
                .map(|c| c.borrow().versioned_name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl Debug for MirNodeType {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        match *self {
            MirNodeType::Aggregation {
                ref on,
                ref group_by,
                ref kind,
            } => {
                let op_string = match *kind {
                    AggregationKind::COUNT => format!("|*|({})", on.name.as_str()),
                    AggregationKind::SUM => format!("𝛴({})", on.name.as_str()),
                };
                let group_cols = group_by
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{} γ[{}]", op_string, group_cols)
            }
            MirNodeType::Base {
                ref column_specs,
                ref keys,
                transactional,
                ..
            } => write!(
                f,
                "B{} [{}; ⚷: {}]",
                if transactional { "*" } else { "" },
                column_specs
                    .into_iter()
                    .map(|&(ref cs, _)| cs.column.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                keys.iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            MirNodeType::Extremum {
                ref on,
                ref group_by,
                ref kind,
            } => {
                let op_string = match *kind {
                    ExtremumKind::MIN => format!("min({})", on.name.as_str()),
                    ExtremumKind::MAX => format!("max({})", on.name.as_str()),
                };
                let group_cols = group_by
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{} γ[{}]", op_string, group_cols)
            }
            MirNodeType::Filter { ref conditions } => {
                use regex::Regex;

                let escape = |s: &str| {
                    Regex::new("([<>])")
                        .unwrap()
                        .replace_all(s, "\\$1")
                        .to_string()
                };
                write!(
                    f,
                    "σ[{}]",
                    conditions
                        .iter()
                        .enumerate()
                        .filter_map(|(i, ref e)| match e.as_ref() {
                            Some(&(ref op, ref x)) => {
                                Some(format!("f{} {} {}", i, escape(&format!("{}", op)), x))
                            }
                            None => None,
                        })
                        .collect::<Vec<_>>()
                        .as_slice()
                        .join(", ")
                )
            }
            MirNodeType::GroupConcat {
                ref on,
                ref separator,
            } => write!(f, "||([{}], \"{}\")", on.name, separator),
            MirNodeType::Identity => write!(f, "≡"),
            MirNodeType::Join {
                ref on_left,
                ref on_right,
                ref project,
            } => {
                let jc = on_left
                    .iter()
                    .zip(on_right)
                    .map(|(l, r)| format!("{}:{}", l.name, r.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "⋈ [{} on {}]",
                    project
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    jc
                )
            }
            MirNodeType::Leaf { ref keys, .. } => {
                let key_cols = keys.iter()
                    .map(|k| k.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "Leaf [⚷: {}]", key_cols)
            }
            MirNodeType::LeftJoin {
                ref on_left,
                ref on_right,
                ref project,
            } => {
                let jc = on_left
                    .iter()
                    .zip(on_right)
                    .map(|(l, r)| format!("{}:{}", l.name, r.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "⋉ [{} on {}]",
                    project
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    jc
                )
            }
            MirNodeType::Latest { ref group_by } => {
                let key_cols = group_by
                    .iter()
                    .map(|k| k.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "⧖ γ[{}]", key_cols)
            }
            MirNodeType::Project {
                ref emit,
                ref literals,
            } => write!(
                f,
                "π [{}{}]",
                emit.iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                if literals.len() > 0 {
                    format!(
                        ", lit: {}",
                        literals
                            .iter()
                            .map(|&(ref n, ref v)| format!("{}: {}", n, v))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                } else {
                    format!("")
                }
            ),
            MirNodeType::Reuse { ref node } => write!(f, "Reuse [{:#?}]", node),
            MirNodeType::TopK {
                ref order, ref k, ..
            } => write!(f, "TopK [k: {}, {:?}]", k, order),
            MirNodeType::Union { ref emit } => {
                let cols = emit.iter()
                    .map(|c| {
                        c.iter()
                            .map(|e| e.name.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .collect::<Vec<_>>()
                    .join(" ⋃ ");

                write!(f, "{}", cols)
            }
        }
    }
}

fn adapt_base_node(
    over_node: MirNodeRef,
    mig: &mut Migration,
    column_specs: &mut [(ColumnSpecification, Option<usize>)],
    add: &Vec<ColumnSpecification>,
    remove: &Vec<ColumnSpecification>,
) -> FlowNode {
    let na = match over_node.borrow().flow_node {
        None => panic!("adapted base node must have a flow node already!"),
        Some(ref flow_node) => flow_node.address(),
    };

    for a in add.iter() {
        let default_value = match a.constraints
            .iter()
            .filter_map(|c| match *c {
                ColumnConstraint::DefaultValue(ref dv) => Some(dv.into()),
                _ => None,
            })
            .next()
        {
            None => DataType::None,
            Some(dv) => dv,
        };
        let column_id = mig.add_column(na, &a.column.name, default_value);

        // store the new column ID in the column specs for this node
        for &mut (ref cs, ref mut cid) in column_specs.iter_mut() {
            if cs == a {
                assert_eq!(*cid, None);
                *cid = Some(column_id);
            }
        }
    }
    for r in remove.iter() {
        let over_node = over_node.borrow();
        let pos = over_node
            .column_specifications()
            .iter()
            .position(|&(ref ecs, _)| ecs == r)
            .unwrap();
        let cid = over_node.column_specifications()[pos]
            .1
            .expect("base column ID must be set to remove column");
        mig.drop_column(na, cid);
    }

    FlowNode::Existing(na)
}

fn make_base_node(
    name: &str,
    column_specs: &mut [(ColumnSpecification, Option<usize>)],
    pkey_columns: &Vec<Column>,
    mig: &mut Migration,
    transactional: bool,
) -> FlowNode {
    // remember the absolute base column ID for potential later removal
    for (i, cs) in column_specs.iter_mut().enumerate() {
        cs.1 = Some(i);
    }

    let column_names = column_specs
        .iter()
        .map(|&(ref cs, _)| &cs.column.name)
        .collect::<Vec<_>>();

    // note that this defaults to a "None" (= NULL) default value for columns that do not have one
    // specified; we don't currently handle a "NOT NULL" SQL constraint for defaults
    let default_values = column_specs
        .iter()
        .map(|&(ref cs, _)| {
            for c in &cs.constraints {
                match *c {
                    ColumnConstraint::DefaultValue(ref dv) => return dv.into(),
                    _ => (),
                }
            }
            return DataType::None;
        })
        .collect::<Vec<DataType>>();

    let base = if pkey_columns.len() > 0 {
        let pkey_column_ids = pkey_columns
            .iter()
            .map(|pkc| {
                //assert_eq!(pkc.table.as_ref().unwrap(), name);
                column_specs
                    .iter()
                    .position(|&(ref cs, _)| cs.column == *pkc)
                    .unwrap()
            })
            .collect();
        ops::base::Base::new(default_values).with_key(pkey_column_ids)
    } else {
        ops::base::Base::new(default_values)
    };

    if transactional {
        FlowNode::New(mig.add_transactional_base(
            name,
            column_names.as_slice(),
            base,
        ))
    } else {
        FlowNode::New(mig.add_ingredient(name, column_names.as_slice(), base))
    }
}

fn make_union_node(
    name: &str,
    columns: &[Column],
    emit: &Vec<Vec<Column>>,
    ancestors: &[MirNodeRef],
    mig: &mut Migration,
) -> FlowNode {
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();
    let mut emit_column_id: HashMap<NodeIndex, Vec<usize>> = HashMap::new();

    // column_id_for_column doesn't take into consideration table aliases
    // which might cause improper ordering of columns in a union node
    // eg. Q6 in finkelstein.txt
    for (i, n) in ancestors.clone().iter().enumerate() {
        let emit_cols = emit[i]
            .iter()
            .map(|c| n.borrow().column_id_for_column(c))
            .collect::<Vec<_>>();

        let ni = n.borrow().flow_node_addr().unwrap();
        emit_column_id.insert(ni, emit_cols);
    }
    let node = mig.add_ingredient(
        String::from(name),
        column_names.as_slice(),
        ops::union::Union::new(emit_column_id),
    );

    FlowNode::New(node)
}

fn make_filter_node(
    name: &str,
    parent: MirNodeRef,
    columns: &[Column],
    conditions: &Vec<Option<(Operator, DataType)>>,
    mig: &mut Migration,
) -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let node = mig.add_ingredient(
        String::from(name),
        column_names.as_slice(),
        ops::filter::Filter::new(parent_na, conditions.as_slice()),
    );
    FlowNode::New(node)
}

fn make_grouped_node(
    name: &str,
    parent: MirNodeRef,
    columns: &[Column],
    on: &Column,
    group_by: &Vec<Column>,
    kind: GroupedNodeType,
    mig: &mut Migration,
) -> FlowNode {
    assert!(group_by.len() > 0);
    assert!(
        group_by.len() <= 4,
        format!(
            "can't have >4 group columns due to compound key restrictions, {} needs {}",
            name,
            group_by.len()
        )
    );

    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let over_col_indx = parent.borrow().column_id_for_column(on);
    let group_col_indx = group_by
        .iter()
        .map(|c| parent.borrow().column_id_for_column(c))
        .collect::<Vec<_>>();

    assert!(group_col_indx.len() > 0);

    let na = match kind {
        GroupedNodeType::Aggregation(agg) => mig.add_ingredient(
            String::from(name),
            column_names.as_slice(),
            agg.over(parent_na, over_col_indx, group_col_indx.as_slice()),
        ),
        GroupedNodeType::Extremum(extr) => mig.add_ingredient(
            String::from(name),
            column_names.as_slice(),
            extr.over(parent_na, over_col_indx, group_col_indx.as_slice()),
        ),
        GroupedNodeType::GroupConcat(sep) => {
            use ops::grouped::concat::{GroupConcat, TextComponent};

            let gc = GroupConcat::new(parent_na, vec![TextComponent::Column(over_col_indx)], sep);
            mig.add_ingredient(String::from(name), column_names.as_slice(), gc)
        }
    };
    FlowNode::New(na)
}


fn make_identity_node(
    name: &str,
    parent: MirNodeRef,
    columns: &[Column],
    mig: &mut Migration,
) -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let node = mig.add_ingredient(
        String::from(name),
        column_names.as_slice(),
        ops::identity::Identity::new(parent_na),
    );
    FlowNode::New(node)
}

fn make_join_node(
    name: &str,
    left: MirNodeRef,
    right: MirNodeRef,
    columns: &[Column],
    on_left: &Vec<Column>,
    on_right: &Vec<Column>,
    proj_cols: &Vec<Column>,
    kind: JoinType,
    mig: &mut Migration,
) -> FlowNode {
    use ops::join::JoinSource;

    assert_eq!(on_left.len(), on_right.len());

    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let projected_cols_left: Vec<Column> = left.borrow()
        .columns
        .iter()
        .filter(|c| proj_cols.contains(c))
        .cloned()
        .collect();
    let projected_cols_right: Vec<Column> = right
        .borrow()
        .columns
        .iter()
        .filter(|c| proj_cols.contains(c))
        .cloned()
        .collect();

    assert_eq!(
        projected_cols_left.len() + projected_cols_right.len(),
        proj_cols.len()
    );

    assert_eq!(on_left.len(), 1, "no support for multiple column joins");
    assert_eq!(on_right.len(), 1, "no support for multiple column joins");

    // this assumes the columns we want to join on appear first in the list
    // of projected columns. this is fine for joins against different tables
    // since we assume unique column names in each table. however, this is
    // not correct for joins against the same table, for example:
    // SELECT r1.a as a1, r2.a as a2 from r as r1, r as r2 where r1.a = r2.b and r2.a = r1.b;
    //
    // the `r1.a = r2.b` join predicate will create a join node with columns: r1.a, r1.b, r2.a, r2,b
    // however, because the way we deal with aliases, we can't distinguish between `r1.a` and `r2.a`
    // at this point in the codebase, so the `r2.a = r1.b` will join on the wrong `a` column.
    let left_join_col_id = projected_cols_left
        .iter()
        .position(|lc| lc == on_left.first().unwrap())
        .unwrap();
    let right_join_col_id = projected_cols_right
        .iter()
        .position(|rc| rc == on_right.first().unwrap())
        .unwrap();

    let join_config = projected_cols_left
        .iter()
        .enumerate()
        .map(|(i, _)| if i == left_join_col_id {
            JoinSource::B(i, right_join_col_id)
        } else {
            JoinSource::L(i)
        })
        .chain(
            projected_cols_right
                .iter()
                .enumerate()
                .map(|(i, _)| JoinSource::R(i)),
        )
        .collect();

    let left_na = left.borrow().flow_node_addr().unwrap();
    let right_na = right.borrow().flow_node_addr().unwrap();

    let j = match kind {
        JoinType::Inner => Join::new(left_na, right_na, JoinType::Inner, join_config),
        JoinType::Left => Join::new(left_na, right_na, JoinType::Left, join_config),
    };
    let n = mig.add_ingredient(String::from(name), column_names.as_slice(), j);

    FlowNode::New(n)
}

fn make_latest_node(
    name: &str,
    parent: MirNodeRef,
    columns: &[Column],
    group_by: &Vec<Column>,
    mig: &mut Migration,
) -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let group_col_indx = group_by
        .iter()
        .map(|c| parent.borrow().column_id_for_column(c))
        .collect::<Vec<_>>();

    let na = mig.add_ingredient(
        String::from(name),
        column_names.as_slice(),
        Latest::new(parent_na, group_col_indx),
    );
    FlowNode::New(na)
}

fn make_project_node(
    name: &str,
    parent: MirNodeRef,
    columns: &[Column],
    emit: &Vec<Column>,
    literals: &Vec<(String, DataType)>,
    mig: &mut Migration,
) -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let projected_column_ids = emit.iter()
        .map(|c| parent.borrow().column_id_for_column(c))
        .collect::<Vec<_>>();

    let (_, literal_values): (Vec<_>, Vec<_>) = literals.iter().cloned().unzip();

    let n = mig.add_ingredient(
        String::from(name),
        column_names.as_slice(),
        Project::new(
            parent_na,
            projected_column_ids.as_slice(),
            Some(literal_values),
        ),
    );
    FlowNode::New(n)
}

fn make_topk_node(
    name: &str,
    parent: MirNodeRef,
    columns: &[Column],
    order: &Option<Vec<(Column, OrderType)>>,
    group_by: &Vec<Column>,
    k: usize,
    offset: usize,
    mig: &mut Migration,
) -> FlowNode {
    let parent_na = parent.borrow().flow_node_addr().unwrap();
    let column_names = columns.iter().map(|c| &c.name).collect::<Vec<_>>();

    let group_by_indx = if group_by.is_empty() {
        // no query parameters, so we index on the first column
        vec![0 as usize]
    } else {
        group_by
            .iter()
            .map(|c| parent.borrow().column_id_for_column(c))
            .collect::<Vec<_>>()
    };

    let cmp_rows = match *order {
        Some(ref o) => {
            assert_eq!(offset, 0); // Non-zero offset not supported

            let columns: Vec<_> = o.iter()
                .map(|&(ref c, ref order_type)| {
                    // SQL and Soup disagree on what ascending and descending order means, so do the
                    // conversion here.
                    let reversed_order_type = match *order_type {
                        OrderType::OrderAscending => OrderType::OrderDescending,
                        OrderType::OrderDescending => OrderType::OrderAscending,
                    };
                    (parent.borrow().column_id_for_column(c), reversed_order_type)
                })
                .collect();

            columns
        }
        None => Vec::new(),
    };

    // make the new operator and record its metadata
    let na = mig.add_ingredient(
        String::from(name),
        column_names.as_slice(),
        ops::topk::TopK::new(parent_na, cmp_rows, group_by_indx, k),
    );
    FlowNode::New(na)
}

fn materialize_leaf_node(node: &MirNodeRef, key_cols: &Vec<Column>, mig: &mut Migration) {
    let na = node.borrow().flow_node_addr().unwrap();

    // we must add a new reader for this query. This also requires adding an identity node (at
    // least currently), since a node can only have a single associated reader. However, the
    // identity node exists at the MIR level, so we don't need to consider it here, as it has
    // already been added.

    // TODO(malte): consider the case when the projected columns need reordering

    if !key_cols.is_empty() {
        // TODO(malte): this does not yet cover the case when there are multiple query
        // parameters, which requires compound key support on Reader nodes.
        //assert_eq!(key_cols.len(), 1);
        let first_key_col_id = node.borrow()
            .column_id_for_column(key_cols.iter().next().unwrap());
        mig.maintain(na, first_key_col_id);
    } else {
        // if no key specified, default to the first column
        mig.maintain(na, 0);
    }
}
