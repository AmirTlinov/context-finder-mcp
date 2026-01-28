use context_search::ContextPackOutput;

pub(super) fn shrink_context_pack_output(inner: &mut ContextPackOutput) -> bool {
    // Under tight budgets, prefer keeping *content* and dropping helper metadata first.
    if !inner.next_actions.is_empty() {
        inner.next_actions.clear();
        return true;
    }
    if inner.items.len() > 1 {
        inner.items.pop();
        inner.budget.dropped_items += 1;
        return true;
    }

    let Some(item) = inner.items.last_mut() else {
        if inner.meta.trust.is_some() {
            inner.meta.trust = None;
            return true;
        }
        if inner.meta.index_state.is_some() {
            inner.meta.index_state = None;
            return true;
        }
        return false;
    };

    // Keep at least one anchor item. Shrink content before giving up.
    if !item.imports.is_empty() {
        item.imports.clear();
        return true;
    }

    if !item.content.is_empty() {
        let target = item.content.len().div_ceil(2);
        let mut cut = target.min(item.content.len());
        while cut > 0 && !item.content.is_char_boundary(cut) {
            cut = cut.saturating_sub(1);
        }
        if cut == 0 {
            item.content.clear();
        } else {
            item.content.truncate(cut);
        }
        return true;
    }

    if item.relationship.is_some() {
        item.relationship = None;
        return true;
    }
    if item.distance.is_some() {
        item.distance = None;
        return true;
    }
    if item.chunk_type.is_some() {
        item.chunk_type = None;
        return true;
    }
    if item.symbol.is_some() {
        item.symbol = None;
        return true;
    }

    if inner.meta.trust.is_some() {
        inner.meta.trust = None;
        return true;
    }
    if inner.meta.index_state.is_some() {
        inner.meta.index_state = None;
        return true;
    }

    false
}
