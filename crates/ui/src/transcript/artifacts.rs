//! Public documents and delivery checklists are reply content, not tool traces.
//! Keep their presentation inside the transcript and reuse its Markdown/fetch state.

use super::*;

impl Transcript {
    pub(super) fn toggle_artifact(
        &mut self,
        row_id: SharedString,
        open: bool,
        cx: &mut Context<Self>,
    ) {
        self.begin_scroll_navigation();
        self.folds.entry(row_id.clone()).or_default().open = Some(!open);
        if let Some(ix) = self.rows.iter().position(|row| row.id == row_id) {
            self.list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    fn artifact_header(
        &self,
        row_id: &SharedString,
        title: SharedString,
        icon: &'static str,
        open: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let toggle_id = row_id.clone();
        div()
            .id(SharedString::from(format!("{row_id}-artifact-header")))
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .py(px(8.0))
            .cursor_pointer()
            .text_size(px(TOOL_TEXT_SIZE))
            .line_height(px(18.0))
            .text_color(theme.text_muted)
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_artifact(toggle_id.clone(), open, cx);
            }))
            .child(
                crate::icons::icon(icon)
                    .size(px(14.0))
                    .text_color(theme.text_muted)
                    .flex_none(),
            )
            .child(div().flex_1().min_w_0().child(title))
            .child(
                crate::icons::icon(if open {
                    crate::icons::ALT_ARROW_DOWN
                } else {
                    crate::icons::ALT_ARROW_RIGHT
                })
                .size(px(14.0))
                .text_color(theme.text_muted)
                .flex_none(),
            )
            .into_any_element()
    }

    pub(super) fn render_document(
        &mut self,
        row: &Row,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let RowKind::Document {
            title,
            preview,
            output_ref,
            resolved,
            is_error,
        } = &row.kind
        else {
            return gpui::Empty.into_any_element();
        };
        let open = self.folds.get(&row.id).and_then(|f| f.open).unwrap_or(true);
        let mut card = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .px(px(12.0))
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.0))
            .child(self.artifact_header(
                &row.id,
                title.clone(),
                crate::icons::DOCUMENT,
                open,
                theme,
                cx,
            ));
        if !open {
            return card.into_any_element();
        }

        // The thin doc carries a preview; the existing output sidecar carries
        // the actual public document. Fetch once when visible, retry explicitly.
        if let Some(blob_ref) = output_ref
            && !self.blob_details.contains_key(blob_ref)
        {
            self.spawn_blob_fetch_with_presentation(
                blob_ref.clone(),
                BlobPresentation::Document,
                cx,
            );
        }
        let fetched = output_ref.as_ref().and_then(|r| self.blob_details.get(r));
        let tree = match fetched {
            Some(BlobFetch::Document(tree)) => tree.clone(),
            _ => preview.clone(),
        };
        let notice = match fetched {
            Some(BlobFetch::Failed) => Some("Couldn't load full document — retry"),
            Some(BlobFetch::Loading(_)) => Some("Loading full document…"),
            _ if *is_error => Some("Document could not be completed"),
            _ if tree.blocks.is_empty() && !resolved => Some("Preparing document…"),
            _ if tree.blocks.is_empty() => Some("No document content"),
            _ => None,
        };
        let failed_fetch = matches!(fetched, Some(BlobFetch::Failed));
        let highlights = self.code_highlight_for(&row.id, &tree, None, cx);
        let mut body = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(render::MD_BLOCK_GAP))
            .pb(px(12.0));
        for (ix, top) in tree.blocks.iter().enumerate() {
            let opts = RenderOptions {
                workspace_root: {
                    let state = self.state.read(cx);
                    self.chat_id
                        .as_deref()
                        .and_then(|id| state.chats.iter().find(|chat| chat.id == id))
                        .or_else(|| state.selected_chat_row())
                        .and_then(|chat| chat.cwd.as_deref())
                        .map(SharedString::from)
                },
                tasks: None, // Read-only; ACP exposes no plan approval/edit API.
                media: None,
                row_key: row.id.clone(),
                veil: None,
                cache: (!render_cache_disabled()).then(|| self.render_cache.clone()),
                now: Instant::now(),
                copy: Some(self.copy_ui_for(&row.id, cx)),
                link: self.workspace_link.clone(),
                code: self.code_uis_for(&row.id, &top.block, ix, cx),
            };
            body = body.child(render::render_block(
                &top.block,
                ix,
                ix,
                &opts,
                theme,
                window,
                highlights
                    .get(&ix)
                    .and_then(|h| h.as_deref())
                    .map(|h| h.lines.as_slice()),
            ));
        }
        if let Some(notice) = notice {
            let mut status = div()
                .id(SharedString::from(format!("{}-document-status", row.id)))
                .text_size(px(TOOL_TEXT_SIZE))
                .text_color(if *is_error {
                    theme.danger
                } else {
                    theme.text_faint
                })
                .child(notice);
            if failed_fetch && let Some(blob_ref) = output_ref.clone() {
                status = status
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.spawn_blob_fetch_with_presentation(
                            blob_ref.clone(),
                            BlobPresentation::Document,
                            cx,
                        );
                    }));
            }
            body = body.child(status);
        }
        card = card.child(body);
        card.into_any_element()
    }

    pub(super) fn render_checklist(
        &self,
        row_id: &SharedString,
        items: &[kratos_proto::TodoItem],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let done = items.iter().filter(|item| item.done).count();
        let open = self
            .folds
            .get(row_id)
            .and_then(|f| f.open)
            .unwrap_or(done < items.len());
        let mut card = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .px(px(12.0))
            .border_1()
            .border_color(theme.border)
            .rounded(px(8.0))
            .child(self.artifact_header(
                row_id,
                format!("Delivery steps · {done}/{} complete", items.len()).into(),
                crate::icons::CHECKLIST,
                open,
                theme,
                cx,
            ));
        if open {
            card = card.child(div().flex().flex_col().gap(px(6.0)).pb(px(10.0)).children(
                items.iter().map(|item| {
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .text_size(px(TOOL_TEXT_SIZE))
                        .line_height(px(20.0))
                        .text_color(if item.done {
                            theme.text_faint
                        } else {
                            theme.text
                        })
                        .child(div().flex_none().w(px(14.0)).child(if item.done {
                            "✓"
                        } else {
                            "○"
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(SharedString::from(item.text.clone())),
                        )
                }),
            ));
        }
        card.into_any_element()
    }
}
