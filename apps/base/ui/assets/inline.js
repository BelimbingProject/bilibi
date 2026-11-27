// Read-first editors. Every commit travels through the queued transport; only its
// own reply ends Saving. Background patches never acknowledge a write.
(() => {
  'use strict';
  const hook = {
    mounted() {
      this.active = false;
      this.draft = null;
      this.pending = new Set();
      this.long = this.el.dataset.hook === 'InlineLongText';
      this.choice = this.el.dataset.hook === 'InlineChoice';
      this.editorSource = this.editor()?.dataset.editor;
      this.date = ['date', 'datetime-local', 'time'].includes(this.el.dataset.inputType);
      this.abort = new AbortController();
      const listen = (node, type, callback) => node.addEventListener(type, callback, {signal: this.abort.signal});
      listen(this.el, 'click', (event) => {
        if (event.target.closest('[data-role="trigger"]')) this.start();
      });
      listen(this.el, 'keydown', (event) => {
        if (event.isComposing || event.defaultPrevented) return;
        if (event.target.closest('[data-role="trigger"]')) {
          if (['Enter', ' ', 'F2'].includes(event.key)) {
            event.preventDefault();
            this.start(undefined, event.key === 'F2');
          } else if (!this.choice && event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
            event.preventDefault();
            this.start(event.key);
          }
        } else if (this.active && event.key === 'Escape') {
          event.preventDefault();
          event.stopPropagation();
          this.cancel(true);
        } else if (this.active && !this.long && event.key === 'Enter') {
          event.preventDefault();
          this.commit(true);
        }
      });
      // A combobox consumes Escape itself; capture still cancels the shell.
      listen(this.el, 'keyup', (event) => {
        if (this.active && event.key === 'Escape' && !event.isComposing) this.cancel(true);
      });
      listen(this.el, 'focusout', (event) => {
        // Focus moving within the editor (a tick in its list) is not leaving
        // it; the microtask below runs before the new element takes focus.
        if (event.relatedTarget && this.editor()?.contains(event.relatedTarget)) return;
        queueMicrotask(() => {
          if (!this.active || this.editor()?.contains(document.activeElement)) return;
          // Several choices are ticked one by one: leaving the editor commits them.
          if (this.choice && !this.ticks()) this.cancel(false);
          else this.commit(false);
        });
      });
      listen(this.el, 'change', (event) => {
        const ticks = this.ticks();
        if (this.active && ticks?.includes(event.target)) {
          this.choiceOrder = (this.choiceOrder || []).filter(code => code !== event.target.value);
          if (event.target.checked) this.choiceOrder.push(event.target.value);
        }
        // A date's picker has no name: the DateInput hook has already copied
        // the chosen day into the named hidden value.
        if ((this.choice || this.date) && !this.ticks() && this.active && (event.target.name === this.el.dataset.field || (this.date && event.target.matches('[data-date-picker]')))) this.commit(true);
      });
      this.updated();
      if (this.el.dataset.initialEdit === 'true') this.start(undefined, false, this.el.dataset.autofocus !== 'false');
    },
    destroyed() { this.abort.abort(); },
    editor() { return this.el.querySelector('[data-role="editor"]'); },
    // A several-choices editor is a MultiSelect named `<field>[]`: its value
    // is the ticked codes joined by commas (a code never holds one).
    ticks() {
      if (!this.choice) return null;
      const name = `${this.el.dataset.field}[]`;
      const boxes = [...(this.editor()?.querySelectorAll('input[type="checkbox"]') || [])].filter((box) => box.name === name);
      return boxes.length || this.editor()?.querySelector('[data-hook="MultiSelect"]') ? boxes : null;
    },
    control() {
      const ticks = this.ticks();
      if (ticks) {
        const owner = this;
        return {
          get value() {
            const checked = ticks.filter(box => box.checked).map(box => box.value);
            const held = owner.choiceOrder || String(owner.el.dataset.value || '').split(',');
            return [...held.filter(code => checked.includes(code)), ...checked.filter(code => !held.includes(code))].join(',');
          },
          set value(text) {
            const codes = String(text || '').split(',');
            for (const box of ticks) {
              if (box.checked !== codes.includes(box.value)) {
                box.checked = codes.includes(box.value);
                box.dispatchEvent(new Event('change', {bubbles: true}));
              }
            }
            owner.choiceOrder = codes.filter(code => ticks.some(box => box.checked && box.value === code));
          },
          setAttribute: () => {}, removeAttribute: () => {},
          focus: () => this.editor().querySelector('[aria-expanded]')?.focus(),
        };
      }
      return [...(this.editor()?.querySelectorAll('[name]') || [])].find((input) => input.name === this.el.dataset.field);
    },
    trigger() { return this.el.querySelector('[data-role="trigger"]'); },
    updated() {
      // Permission loss removes the editor on the server and discards its draft.
      if (this.el.dataset.editable !== 'true') {
        this.active = false;
        this.draft = null;
      }
      // The patcher updates data-* on the retained editor. Follow the owner's
      // latest render when idle; an active/refused/pending editor stays intact.
      // Only its matching successful reply or cancellation releases a draft.
      const editor = this.editor();
      const source = editor?.dataset.editor;
      if (this.choice && editor && !this.active && !this.draft && !this.pending.size
        && source !== undefined && source !== this.editorSource) {
        const template = document.createElement('template');
        template.innerHTML = source;
        this.editorSource = source;
        editor.replaceChildren(template.content);
      }
      // The owner's hint, then the outcome, as the template renders them.
      const describedBy = [
        this.el.querySelector('[data-role="hint"]') && `${this.el.id}-hint`,
        this.el.dataset.status !== 'none' && `${this.el.id}-status`,
      ].filter(Boolean).join(' ');
      const trigger = this.trigger();
      if (trigger) {
        trigger.classList.toggle(this.long || this.choice ? 'hidden' : 'invisible', this.active);
        trigger.classList.toggle('pointer-events-none', this.active);
        if (describedBy) trigger.setAttribute('aria-describedby', describedBy);
        else trigger.removeAttribute('aria-describedby');
      }
      this.editor()?.classList.toggle('hidden', !this.active);
      const input = this.control();
      if (input) {
        input.setAttribute('aria-invalid', String(['error', 'conflict'].includes(this.el.dataset.status)));
        if (describedBy) input.setAttribute('aria-describedby', describedBy);
        else input.removeAttribute('aria-describedby');
      }
      // Only committed server data paints the fact, even on failure.
      const text = this.el.querySelector('[data-role="text"]');
      if (text) {
        const value = this.el.dataset.displayValue ?? this.el.dataset.value;
        // The field's own empty text, never the editor's hint: a date box's
        // placeholder is its format ("d mmm yyyy"), not the fact's value.
        const placeholder = this.el.dataset.placeholder || '—';
        // The source HEEx wraps a long value in line breaks inside its
        // pre-wrap span; a fact shows the value where its field's text sits.
        const displayed = value || placeholder;
        if (text.textContent !== displayed) text.textContent = displayed;
        text.classList.toggle('text-ink-muted', !value);
        text.classList.toggle('text-ink', !!value);
      }
      this.el.toggleAttribute('aria-busy', this.pending.size > 0);
      if (this.pending.size) this.el.setAttribute('aria-busy', 'true');
      this.el.querySelector('[data-role="saving"]')?.classList.toggle('hidden', !this.pending.size);
    },
    start(value, atEnd = false, autofocus = true) {
      if (this.el.dataset.editable !== 'true' || this.active) return;
      this.active = true;
      const current = this.el.dataset.value;
      const draft = this.draft || {
        value: current,
        original: current,
        timezone: this.el.dataset.timezone,
        version: this.el.dataset.version,
      };
      this.original = draft.original;
      this.timezone = draft.timezone;
      this.version = draft.version;
      this.openedPendingValue = this.pending.has(draft) ? draft.value : undefined;
      const input = this.control();
      if (!input) { this.active = false; return; }
      input.value = value === undefined ? draft.value : value;
      const combo = this.editor().querySelector('[data-hook="Combobox"]');
      combo?.dispatchEvent(new CustomEvent('bilibi:choice-value', {detail: {value: input.value}}));
      // A date's named input is the hidden value; its typed box shows it.
      const dateBox = this.editor().querySelector('[data-hook="DateInput"]');
      dateBox?.dispatchEvent(new CustomEvent('bilibi:date-value', {detail: {value: input.value}}));
      this.editor().querySelector('[data-hook="SwitchInput"]')?.dispatchEvent(new Event('bilibi:switch-value'));
      this.updated();
      if (!autofocus) return;
      const focus = this.editor().querySelector('[role="combobox"], [role="switch"]') || dateBox?.querySelector('[data-role="input"]') || input;
      focus.focus();
      if (focus.select) {
        if (value !== undefined || atEnd) focus.setSelectionRange(focus.value.length, focus.value.length);
        else focus.select();
      }
    },
    cancel(returnFocus) {
      this.active = false;
      this.draft = null;
      this.message('');
      this.updated();
      if (returnFocus) this.trigger()?.focus();
    },
    commit(returnFocus) {
      if (!this.active) return;
      const input = this.control();
      const value = this.long || this.choice ? input.value : input.value.trim();
      const unchanged = value === this.openedPendingValue
        || (value === this.original && !this.pending.size);
      if (unchanged || (!value.trim() && !this.el.hasAttribute('data-allow-empty'))) {
        this.cancel(returnFocus);
        return;
      }
      const draft = {value, original: this.original, timezone: this.timezone, version: this.version};
      this.draft = draft;
      this.active = false;
      this.pending.add(draft);
      this.message('');
      this.updated();
      if (returnFocus) this.trigger()?.focus();
      this.emit(this.el.dataset.saveEvent, {
        id: this.el.dataset.id, field: this.el.dataset.field, value,
        fact: this.el.id, original: draft.original,
        ...(draft.timezone ? {timezone: draft.timezone} : {}),
        ...(draft.version ? {version: draft.version} : {}),
        prepareOriginal: () => draft.original,
        onReply: (reply) => {
          this.pending.delete(draft);
          if (!this.el.isConnected) return;
          if (reply.type === 'error') {
            if (this.draft === draft) this.message(reply.message || 'The change was not confirmed. Open the field to retry, or press Escape to discard the draft.');
          } else if (this.el.dataset.status === 'saved') {
            if (this.el.dataset.value === draft.value && this.el.dataset.timezone === draft.timezone) {
              if (this.active && this.timezone === draft.timezone && this.original === draft.original) this.original = draft.value;
              for (const pending of this.pending) {
                if (pending.timezone === draft.timezone && pending.original === draft.original) pending.original = draft.value;
              }
            }
            if (this.draft === draft) this.draft = null;
          } else if (this.draft === draft && this.el.dataset.status === 'conflict') {
            this.draft = null;
          }
          this.updated();
        },
      });
    },
    message(value) {
      const message = this.el.querySelector('[data-role="connection"]');
      if (!message) return;
      message.textContent = value;
      message.classList.toggle('hidden', !value);
    },
  };
  for (const name of ['InlineEdit', 'InlineLongText', 'InlineChoice']) window.BilibiUI.registerHook(name, hook);
})();
