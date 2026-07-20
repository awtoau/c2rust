use super::Translation;
use crate::c_ast::iterators::{immediate_children_all_types, NodeVisitor, SomeId};
use crate::c_ast::{CDeclId, CDeclKind, CommentContext, SrcLoc, TypedAstContext};
use crate::rust_ast::comment_store::CommentStore;
use crate::rust_ast::{pos_to_span, SpanExt};
use log::debug;
use proc_macro2::Span;
use std::collections::{HashMap, HashSet};

struct CommentLocator<'c> {
    ast_context: &'c TypedAstContext,
    comment_context: &'c CommentContext,
    comment_store: &'c mut CommentStore,
    spans: &'c mut HashMap<SomeId, Span>,
    top_decls: &'c HashSet<CDeclId>,
    last_id: Option<SomeId>,
}

impl<'c> CommentLocator<'c> {
    /// Check for comments starting on the same line but after the end of the
    /// last node and before the end of the current node.
    fn check_last_for_trailing(&mut self, cur_loc: SrcLoc) {
        let last_id = match self.last_id {
            // We can only attach trailing comments to the end of statements
            // currently. The pretty-printer only supports trailing comments on
            // statements and comma-separated exprs, but we don't support
            // comments after comma-separated exprs yet.
            Some(SomeId::Stmt(id)) => SomeId::Stmt(id),
            _ => return,
        };
        if let Some(last_loc) = self.ast_context.get_src_loc(last_id) {
            // TODO: handle Mixed comments (code before and after the
            // comment on the same line
            if cur_loc.line == last_loc.end_line {
                return;
            }

            while let Some(comment) = self
                .comment_context
                .peek_next_comment_on_line(last_loc.end(), self.ast_context)
            {
                if comment.loc.unwrap().end() < cur_loc {
                    let existing_pos = self.spans.get(&last_id).map(|span| span.lo());
                    if let Some(pos) = self.comment_store.extend_existing_comments(
                        &[comment.kind.clone()],
                        existing_pos,
                        //CommentStyle::Trailing,
                    ) {
                        debug!(
                            "Attaching comment {:?} to end of line at pos {:?}",
                            comment.kind, pos
                        );
                        // Add the span if we haven't already
                        self.spans
                            .entry(last_id)
                            .or_insert_with(|| pos_to_span(pos));
                    }
                    let file = self
                        .ast_context
                        .file_id(&comment)
                        .expect("All comments must have a source location");
                    self.comment_context.advance_comment(file);
                } else {
                    break;
                }
            }
        }
    }
}

impl<'c> NodeVisitor for CommentLocator<'c> {
    fn children(&mut self, id: SomeId) -> Vec<SomeId> {
        immediate_children_all_types(self.ast_context, id)
    }

    /// A not-yet-processed top-level decl reached early as a reference (e.g.
    /// a forward reference from an earlier top-level decl's subtree) must
    /// stay eligible for a full, proper visit when its own turn as a root
    /// comes around later in `locate_comments`'s loop — see `pre`/`post`'s
    /// matching `top_decls.contains` guards, which no-op for exactly this
    /// case. Without this, sharing one `visited` set across every top-level
    /// decl's traversal (awtoau/c2rust#4) would permanently mark such a
    /// decl "visited" the first time it's merely glimpsed, and it would
    /// never be entered/walked at all when it becomes a root.
    fn defer_visited(&mut self, id: SomeId) -> bool {
        matches!(id, SomeId::Decl(decl_id) if self.top_decls.contains(&decl_id))
    }

    fn pre(&mut self, mut id: SomeId) -> bool {
        if !self.ast_context.contains_node(id) {
            return false;
        }

        // Don't traverse into unvisited top-level decls, we should visit those
        // in sorted order.
        if let SomeId::Decl(id) = id {
            if self.top_decls.contains(&id) {
                return false;
            }
        }

        if let Some(loc) = self.ast_context.get_src_loc(id) {
            // Check if we have a comment before this node that we need to
            // attach to the end of the last node.
            self.check_last_for_trailing(loc.begin());

            let comments = self
                .comment_context
                .get_comments_before(loc.begin(), self.ast_context);
            if let SomeId::Decl(decl_id) = id {
                let decl_kind = &self.ast_context[decl_id].kind;
                if let CDeclKind::NonCanonicalDecl { canonical_decl } = decl_kind {
                    // Attach non-canonical decl comments to their canonical
                    // declaration
                    id = SomeId::Decl(*canonical_decl);
                }
            }
            if let Some(existing) = self.spans.get(&id) {
                let new_pos = self.comment_store.extend_existing_comments(
                    &comments,
                    Some(existing.lo()),
                    //CommentStyle::Isolated,
                );
                debug!(
                    "Attaching more comments {:?} to id {:?} at pos {:?}",
                    comments, id, new_pos
                );
            } else if let Some(pos) = self.comment_store.add_comments(&comments) {
                debug!(
                    "Attaching comments {:?} to id {:?} at pos {:?}",
                    comments, id, pos
                );
                let span = pos_to_span(pos);
                self.spans.insert(id, span);
            }
        }

        // Don't traverse into macro object replacement expressions, as they are
        // in other places.
        if let SomeId::Decl(id) = id {
            if let CDeclKind::MacroObject { .. } = self.ast_context[id].kind {
                return false;
            }
        }

        true
    }

    fn post(&mut self, id: SomeId) {
        if !self.ast_context.contains_node(id) {
            return;
        }

        // Don't attach comments to the end of unvisited top-level decls, we'll
        // visit them later.
        if let SomeId::Decl(id) = id {
            if self.top_decls.contains(&id) {
                return;
            }
        }
        if let Some(loc) = self.ast_context.get_src_loc(id) {
            let comments = self
                .comment_context
                .get_comments_before(loc.end(), self.ast_context);
            if let Some(pos) = self.comment_store.add_comments(&comments) {
                debug!(
                    "Attaching comments {:?} to end of id {:?} at pos {:?}",
                    comments, id, pos
                );
                let span = self.spans.entry(id).or_insert_with(Span::call_site);
                *span = span.with_hi(pos);
            }

            // Check if we have a comment before the end of this node that we
            // need to attach to the end of the last node.
            self.check_last_for_trailing(loc.end());
        }

        self.last_id = Some(id);
    }
}

impl<'c> Translation<'c> {
    /// Create spans for each C AST node that has a comment attached to it.
    ///
    /// This walks every top-level declaration's subtree in source order
    /// (matching `c_decls_top`'s order), but shares a single `visited` set
    /// and a single `last_id` cursor across *all* of those per-decl walks
    /// (via `CommentLocator::visit_tree_shared`), rather than giving each
    /// one a fresh `visited: HashSet` (see `NodeVisitor::visit_tree`).
    ///
    /// Any subtree reachable from more than one top-level decl (routine for
    /// widely-used types in a large header closure) would otherwise be
    /// walked once per top-level decl that reaches it — O(N x
    /// shared-subtree-size) — instead of once total. See awtoau/c2rust#4.
    ///
    /// Sharing `visited` alone is safe for `NodeVisitor::pre`/`post`'s
    /// comment-attachment side effects: `spans` is already a single map
    /// threaded across every top-level decl's traversal (both before and
    /// after this change), so attaching to the same `SomeId` from a later
    /// decl's walk is the same idempotent `spans.entry(id)` update either
    /// way. `visit_tree`'s own stack machine already calls `post()` (and
    /// thus updates `last_id`) on a node reached a *second* time within a
    /// single walk without re-running `pre()` or descending again — a
    /// shared `visited` set just makes "second time" also cover nodes
    /// entered under an *earlier* top-level decl's walk, which is the same
    /// case, not a new one.
    ///
    /// `last_id` no longer resets to `None` at each top-level decl
    /// boundary; it persists as "the last node visited overall" so
    /// `check_last_for_trailing` keeps working across the seam between one
    /// top-level decl's traversal and the next, exactly as it already does
    /// across node boundaries within a single decl's traversal.
    pub fn locate_comments(&mut self) {
        let mut top_decls: HashSet<CDeclId> =
            self.ast_context.c_decls_top.iter().copied().collect();
        let mut spans: HashMap<SomeId, Span> = HashMap::new();
        let mut visited: HashSet<SomeId> = HashSet::new();
        let mut last_id: Option<SomeId> = None;
        for decl_id in &self.ast_context.c_decls_top {
            top_decls.remove(decl_id);
            let mut visitor = CommentLocator {
                ast_context: &self.ast_context,
                comment_context: &self.comment_context,
                comment_store: &mut self.comment_store.borrow_mut(),
                spans: &mut spans,
                top_decls: &top_decls,
                last_id,
            };
            visitor.visit_tree_shared(SomeId::Decl(*decl_id), &mut visited);
            last_id = visitor.last_id;
        }
        self.spans = spans;
    }

    pub fn get_span(&self, id: SomeId) -> Option<Span> {
        self.spans.get(&id).copied()
    }
}
