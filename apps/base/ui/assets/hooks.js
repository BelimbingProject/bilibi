// Browser behaviour for the shared form controls in templates/forms.html:
// CodeInput, ColourInput, Combobox, SecretReveal, SecretClear, SecretStored and
// DisclosureDismiss, MultiSelect, LocationAction, FilterSheet, DateInput (plus the dashboard's DashboardSort and the form Fold).
// The LiveView JS commands are folded into the hooks.
// Each registers with the shared `data-hook` runtime in ui.js
// (BilibiUI.registerHook), which mounts a control when it enters the
// document, calls updated when its subtree is patched and destroyed when it
// leaves. The server owns every value and the option vocabulary; a hook owns
// only transient state (open, query, caret, masked-or-shown). A hook reports
// intent with `this.emit(event, values)`, the runtime's bubbling
// `bilibi:event` CustomEvent, and hears a server push as a `window` event
// `bilibi:<name>` with the payload in detail. ui.js must load first.
(() => {
  // The server owns opening/closing; this hook owns keyboard and focus only.
  window.BilibiUI.registerHook("InlineForm", {
    mounted() {
      this.keydown = event => {
        if (this.el.dataset.editing !== "true") return
        if (event.key === "Escape") {
          event.preventDefault()
          event.stopPropagation()
          this.emit(this.el.dataset.cancelEvent, {key: this.el.dataset.key})
        } else if (event.key === "Enter" && !event.isComposing &&
                   event.target.matches("input:not([type=button]):not([type=submit])")) {
          event.preventDefault()
          this.el.querySelector("form")?.requestSubmit()
        }
      }
      this.el.addEventListener("keydown", this.keydown)
      this.updated()
    },
    updated() {
      const editing = this.el.dataset.editing === "true"
      if (editing !== this.editing) {
        this.editing = editing
        const target = editing ? this.el.querySelector("input:not([type=hidden]):not([disabled]), select, textarea") : this.el.querySelector("button")
        if (editing || this.wasEditing) target?.focus()
        this.wasEditing = editing
      }
    },
    destroyed() { this.el.removeEventListener("keydown", this.keydown) },
  })

  // One native input is the keyboard and accessibility contract. Decorative
  // boxes follow its caret, so editing never adds eight tab stops or labels.
  // Methods own their code formats; the form still posts one credential to
  // its owner. Native FormData keeps the method choice out of event schemas.
  const FactorProof = {
    mounted() {
      this.form = this.el.closest("form")
      this.select = this.el.querySelector("select")
      this.choose = () => {
        const groups = [...this.el.querySelectorAll("[data-proof-method]")]
        const selected = this.select?.value || groups[0]?.dataset.proofMethod
        for (const group of groups) {
          group.hidden = group.dataset.proofMethod !== selected
          group.disabled = group.hidden
        }
      }
      this.qualify = event => {
        const active = this.el.querySelector("[data-proof-method]:not([disabled])")
        const name = this.el.dataset.proofName
        const code = event.formData.get(name)
        if (active && typeof code === "string" && code.trim()) {
          event.formData.set(name, `${active.dataset.proofMethod}:${code}`)
        }
      }
      this.select?.addEventListener("change", this.choose)
      this.form?.addEventListener("formdata", this.qualify)
      this.choose()
    },
    updated() { this.choose() },
    destroyed() {
      this.select?.removeEventListener("change", this.choose)
      this.form?.removeEventListener("formdata", this.qualify)
    },
  }

  const CodeInput = {
    mounted() {
      this.input = this.el.querySelector("input")
      this.boxes = [...this.el.querySelectorAll("[data-code-index]")]
      this.length = Number(this.el.dataset.codeLength)
      this.normalize = value => (this.el.hasAttribute("data-code-numeric") ?
        value.replace(/[^0-9]/g, "") : value.toUpperCase().replace(/[\s-]/g, "")).slice(0, this.length)
      // Writes only what changed: the runtime reports a child mutation inside
      // the control as an update, and rewriting an unchanged text node would
      // report another.
      this.paint = () => {
        const active = Math.min(this.input.selectionStart ?? 0, this.length - 1)
        this.boxes.forEach((box, index) => {
          const text = this.input.value[index] || ""
          if (box.textContent !== text) box.textContent = text
          box.toggleAttribute("data-active", document.activeElement === this.input &&
            (index === active || (index >= this.input.selectionStart && index < this.input.selectionEnd)))
          box.toggleAttribute("data-invalid", this.input.getAttribute("aria-invalid") === "true")
        })
      }
      this.onInput = () => {
        const caret = this.normalize(this.input.value.slice(0, this.input.selectionStart)).length
        this.input.value = this.normalize(this.input.value)
        this.input.setSelectionRange(caret, caret)
        this.paint()
      }
      this.onPaste = event => {
        const text = event.clipboardData?.getData("text")
        if (text == null) return
        event.preventDefault()
        const start = this.input.selectionStart
        const end = this.input.selectionEnd
        const code = this.normalize(text)
        // A whole code replaces the field even when pasted midway through it.
        this.input.value = code.length === this.length ? code :
          this.normalize(this.input.value.slice(0, start) + code + this.input.value.slice(end))
        const caret = code.length === this.length ? this.length : Math.min(start + code.length, this.length)
        this.input.setSelectionRange(caret, caret)
        this.input.dispatchEvent(new Event("input", {bubbles: true}))
      }
      this.onClick = event => {
        // The transparent native input remains hit-testable for touch paste.
        // Preserve ranges created by dragging, Shift-click or multi-click.
        if (event.target === this.input && (this.input.selectionStart !== this.input.selectionEnd ||
          event.shiftKey || event.detail > 1)) { this.paint(); return }
        const box = event.target === this.input ? this.boxes.find(box => {
          const rect = box.getBoundingClientRect()
          return event.clientX >= rect.left && event.clientX <= rect.right
        }) : event.target.closest("[data-code-index]")
        if (!box) return
        this.input.focus()
        const index = Math.min(Number(box.dataset.codeIndex), this.input.value.length)
        this.input.setSelectionRange(index, index)
        this.paint()
      }
      this.input.value = this.normalize(this.input.value)
      this.input.classList.add("absolute", "inset-0", "w-full", "opacity-0")
      this.el.querySelector("[data-code-boxes]").hidden = false
      this.input.addEventListener("input", this.onInput)
      this.input.addEventListener("paste", this.onPaste)
      for (const event of ["focus", "blur", "keyup", "select"]) this.input.addEventListener(event, this.paint)
      this.el.addEventListener("click", this.onClick)
      this.paint()
    },
    updated() {
      this.input.classList.add("absolute", "inset-0", "w-full", "opacity-0")
      this.el.querySelector("[data-code-boxes]").hidden = false
      this.input.value = this.normalize(this.input.value)
      this.paint()
    },
    destroyed() {
      this.input.removeEventListener("input", this.onInput)
      this.input.removeEventListener("paste", this.onPaste)
      for (const event of ["focus", "blur", "keyup", "select"]) this.input.removeEventListener(event, this.paint)
      this.el.removeEventListener("click", this.onClick)
    },
  }

  // Select-only colour combobox. The native select is the only submitted
  // control, including when JavaScript is unavailable. The closed field and
  // each option show the name inside that colour's banner chip. Unlike a
  // searchable combobox, typing moves focus among named colours without
  // editing the value.
  const ColourInput = {
    mounted() {
      this.select = this.el.querySelector('[data-colour-select]')
      this.trigger = this.el.querySelector('[data-colour-trigger]')
      this.list = this.el.querySelector('[role="listbox"]')
      this.label = this.el.querySelector('label')
      this.open = false
      this.active = 0
      this.query = ''
      this.options = () => Array.from(this.list.querySelectorAll('[role="option"]'))
      this.sync = () => {
        this.select.hidden = true
        this.trigger.hidden = false
        this.trigger.disabled = this.select.matches(':disabled') || this.select.hasAttribute('readonly')
        if (this.trigger.disabled) this.open = false
        this.label.htmlFor = this.trigger.id
        const options = this.options()
        const selected = options.find(option => option.dataset.value === this.select.value) || options[0]
        // The shared observer calls updated after child changes. Keep this
        // repaint idempotent or an unchanged preview would update forever.
        const preview = this.el.querySelector('[data-colour-preview]')
        const chip = selected.querySelector('[data-option-label]')
        const text = chip.textContent
        if (preview.textContent !== text) preview.textContent = text
        if (preview.className !== chip.className) preview.className = chip.className
        options.forEach(option => option.setAttribute('aria-selected', String(option === selected)))
        if (!this.open) this.active = options.indexOf(selected)
        this.active = Math.min(this.active, options.length - 1)
        this.paint()
      }
      this.paint = () => {
        this.trigger.setAttribute('aria-expanded', String(this.open))
        this.list.hidden = !this.open
        this.options().forEach((option, index) => option.toggleAttribute('data-active', this.open && index === this.active))
        if (this.open) {
          const option = this.options()[this.active]
          this.trigger.setAttribute('aria-activedescendant', option.id)
          option.scrollIntoView({block: 'nearest'})
        } else this.trigger.removeAttribute('aria-activedescendant')
      }
      this.setOpen = open => {
        this.open = open && !this.trigger.disabled
        this.query = ''
        this.active = Math.max(0, this.options().findIndex(option => option.dataset.value === this.select.value))
        this.paint()
      }
      this.commit = option => {
        if (this.trigger.disabled) return
        this.select.value = option.dataset.value
        this.setOpen(false)
        this.sync()
        this.select.dispatchEvent(new Event('input', {bubbles: true}))
        this.select.dispatchEvent(new Event('change', {bubbles: true}))
      }
      this.onClick = () => this.setOpen(!this.open)
      this.onKey = event => {
        const count = this.options().length
        if (['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
          event.preventDefault()
          const wasOpen = this.open
          if (!wasOpen) this.setOpen(true)
          if (event.key === 'Home') this.active = 0
          else if (event.key === 'End') this.active = count - 1
          else if (wasOpen) this.active = (this.active + (event.key === 'ArrowDown' ? 1 : -1) + count) % count
          this.paint()
        } else if (event.key === 'Enter' || event.key === ' ') {
          event.preventDefault()
          if (this.open) this.commit(this.options()[this.active])
          else this.setOpen(true)
        } else if (event.key === 'Escape' && this.open) {
          event.preventDefault()
          event.stopPropagation()
          this.setOpen(false)
        } else if (event.key === 'Tab') this.setOpen(false)
        else if (event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
          event.preventDefault()
          if (!this.open) this.setOpen(true)
          clearTimeout(this.queryTimer)
          this.query += event.key.toLocaleLowerCase()
          const match = this.options().findIndex(option => option.textContent.trim().toLocaleLowerCase().startsWith(this.query))
          if (match >= 0) this.active = match
          this.paint()
          this.queryTimer = setTimeout(() => { this.query = '' }, 700)
        }
      }
      this.onOption = event => {
        const option = event.target.closest('[role="option"]')
        if (option && this.list.contains(option)) this.commit(option)
      }
      this.onPointer = event => {
        if (event.target.closest('[role="option"]')) event.preventDefault()
      }
      this.onOutside = event => { if (!this.el.contains(event.target)) this.setOpen(false) }
      this.onBlur = event => { if (!this.el.contains(event.relatedTarget)) this.setOpen(false) }
      this.onReset = () => { this.resetTimer = setTimeout(() => { this.setOpen(false); this.sync() }, 0) }
      this.onChange = () => this.sync()
      this.form = this.select.form
      this.trigger.addEventListener('click', this.onClick)
      this.trigger.addEventListener('keydown', this.onKey)
      this.list.addEventListener('pointerdown', this.onPointer)
      this.list.addEventListener('click', this.onOption)
      this.el.addEventListener('focusout', this.onBlur)
      this.select.addEventListener('change', this.onChange)
      this.form?.addEventListener('reset', this.onReset)
      document.addEventListener('pointerdown', this.onOutside)
      this.sync()
    },
    updated() { this.sync() },
    destroyed() {
      clearTimeout(this.queryTimer)
      clearTimeout(this.resetTimer)
      this.trigger.removeEventListener('click', this.onClick)
      this.trigger.removeEventListener('keydown', this.onKey)
      this.list.removeEventListener('pointerdown', this.onPointer)
      this.list.removeEventListener('click', this.onOption)
      this.el.removeEventListener('focusout', this.onBlur)
      this.select.removeEventListener('change', this.onChange)
      this.form?.removeEventListener('reset', this.onReset)
      document.removeEventListener('pointerdown', this.onOutside)
    }
  }

  // Client-side filtering and keyboard interaction for the combobox.
  //
  // The server owns the hidden form value and the option vocabulary. This hook
  // owns only the transient query, open state and active option, so typing
  // does not cause a round trip or lose the caret: the filter input's own
  // input and change events stop here, before an enclosing form hears them. A
  // choice is committed by copying its value to the hidden input and
  // dispatching a normal bubbling change event; forms therefore keep their
  // existing change and submit contracts. Typing highlights the first match,
  // and Enter while the list is open commits the highlighted option or, with
  // none, does nothing, so it never submits the form.
  const Combobox = {
    mounted() {
      this.input = this.el.querySelector('[role="combobox"]')
      this.valueInput = document.getElementById(this.el.dataset.valueId)
      this.listbox = this.el.querySelector('[role="listbox"]')
      this.noMatches = this.el.querySelector('[id$="-no-matches"]')
      this.clearButton = this.el.querySelector('[id$="-clear"]')
      this.open = false
      this.editing = false
      this.activeIndex = -1
      this.selectedValue = this.valueInput?.value || ""
      this.selectedLabel = this.input?.value || ""

      this.options = () => Array.from(this.listbox?.querySelectorAll('[role="option"]') || [])

      this.filteredOptions = () => this.options().filter(option => !option.hidden)

      this.setOpen = open => {
        this.open = open
        this.input.setAttribute("aria-expanded", String(open))
        this.listbox.hidden = !open

        if (!open) {
          this.input.removeAttribute("aria-activedescendant")
          this.activeIndex = -1
        }
      }

      this.applyFilter = () => {
        const selectionIsActive = this.input.selectionStart === 0 &&
          this.input.selectionEnd === this.input.value.length
        const query = selectionIsActive ? "" : this.input.value.trim().toLocaleLowerCase()
        const options = this.options()

        options.forEach(option => {
          const label = option.dataset.label.toLocaleLowerCase()
          const value = option.dataset.value.toLocaleLowerCase()
          option.hidden = query !== "" && !label.includes(query) && !value.includes(query)
        })

        const filtered = this.filteredOptions()
        this.noMatches.hidden = options.length === 0 || filtered.length !== 0

        if (this.activeIndex >= filtered.length) this.activeIndex = filtered.length - 1
        this.renderActive()
      }

      this.renderActive = () => {
        const filtered = this.filteredOptions()

        this.options().forEach(option => {
          const active = filtered[this.activeIndex] === option
          option.classList.toggle("bg-brand-surface", active)
          option.setAttribute("aria-selected", String(option.dataset.value === this.selectedValue))
        })

        const active = filtered[this.activeIndex]
        if (active) {
          this.input.setAttribute("aria-activedescendant", active.id)
          active.scrollIntoView({block: "nearest"})
        } else {
          this.input.removeAttribute("aria-activedescendant")
        }
      }

      this.openList = selectQuery => {
        if (!this.open) {
          this.setOpen(true)
          this.activeIndex = -1
          if (selectQuery) this.input.select()
        }

        this.applyFilter()
      }

      // The box shows the name with its code muted and italic through a
      // display layer over the plain input, which stays the accessible value,
      // until the person edits (`data-editing`). Write only on change: a hook child-list write reports `updated`.
      this.setEditing = editing => {
        this.editing = editing
        if (this.el.hasAttribute("data-editing") !== editing) {
          this.el.toggleAttribute("data-editing", editing)
        }
      }

      this.renderDisplay = option => {
        this.setEditing(false)
        const display = this.el.querySelector("[data-combobox-display]")
        if (!display) return
        const [name, detail] = display.children
        const nextName = option?.dataset.name || ""
        const nextDetail = option?.dataset.detail || ""
        if (name.textContent !== nextName) name.textContent = nextName
        if (detail.textContent !== nextDetail) detail.textContent = nextDetail
        if (this.el.hasAttribute("data-detailed") !== (nextDetail !== "")) {
          this.el.toggleAttribute("data-detailed", nextDetail !== "")
        }
      }

      this.restore = () => {
        this.selectedValue = this.valueInput?.value || ""
        const selected = this.options().find(option => option.dataset.value === this.selectedValue)
        this.selectedLabel = selected?.dataset.label || ""
        this.input.value = this.selectedLabel
        this.renderDisplay(selected)
      }

      this.close = () => {
        this.restore()
        this.setOpen(false)
      }

      this.commit = option => {
        if (!option) return

        this.selectedValue = option.dataset.value
        this.selectedLabel = option.dataset.label
        this.input.value = this.selectedLabel
        this.valueInput.value = this.selectedValue
        this.renderDisplay(option)
        this.setOpen(false)
        this.valueInput.dispatchEvent(new Event("input", {bubbles: true}))
        this.valueInput.dispatchEvent(new Event("change", {bubbles: true}))
      }

      this.cancel = () => {
        const wasOpen = this.open
        this.close()

        if (!wasOpen || !this.el.dataset.cancelEvent) return

        this.emit(this.el.dataset.cancelEvent)
      }

      this.onInput = event => {
        event.stopPropagation()
        this.setEditing(true)
        if (!this.open) this.setOpen(true)
        this.activeIndex = 0
        this.applyFilter()
      }
      this.onChange = event => event.stopPropagation()
      this.onFocus = () => this.openList(true)
      this.onKeyDown = event => {
        const filtered = this.filteredOptions()

        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          event.preventDefault()
          this.openList(false)
          const delta = event.key === "ArrowDown" ? 1 : -1
          this.activeIndex = filtered.length === 0
            ? -1
            : this.activeIndex < 0
              ? (delta === 1 ? 0 : filtered.length - 1)
              : (this.activeIndex + delta + filtered.length) % filtered.length
          this.renderActive()
        } else if (event.key === "Enter") {
          if (this.open) {
            event.preventDefault()
            this.commit(filtered[this.activeIndex])
          }
        } else if (event.key === "Escape") {
          if (this.open) {
            event.preventDefault()
            this.cancel()
          }
        }
      }

      this.onOptionMouseDown = event => {
        event.preventDefault()
        this.commit(event.currentTarget)
      }

      this.bindOptions = () => {
        this.options().forEach(option => {
          option.removeEventListener("mousedown", this.onOptionMouseDown)
          option.addEventListener("mousedown", this.onOptionMouseDown)
        })
      }

      this.bindClear = () => {
        this.clearButton?.removeEventListener("mousedown", this.onClearMouseDown)
        this.clearButton?.removeEventListener("click", this.onClear)
        this.clearButton = this.el.querySelector('[id$="-clear"]')
        this.clearButton?.addEventListener("mousedown", this.onClearMouseDown)
        this.clearButton?.addEventListener("click", this.onClear)
      }

      this.onDocumentPointerDown = event => {
        if (!this.el.contains(event.target) && this.open) this.cancel()
      }

      this.onFocusOut = event => {
        if (event.relatedTarget && this.el.contains(event.relatedTarget)) return
        requestAnimationFrame(() => {
          if (this.open && !this.el.contains(document.activeElement)) this.cancel()
        })
      }

      this.onClear = event => {
        event.preventDefault()
        this.selectedValue = ""
        this.selectedLabel = ""
        this.input.value = ""
        this.valueInput.value = ""
        this.renderDisplay(null)
        this.valueInput.dispatchEvent(new Event("input", {bubbles: true}))
        this.valueInput.dispatchEvent(new Event("change", {bubbles: true}))
        this.input.focus()
        this.openList(false)
      }

      this.onClearMouseDown = event => event.preventDefault()

      this.input.addEventListener("input", this.onInput)
      this.input.addEventListener("change", this.onChange)
      this.input.addEventListener("focus", this.onFocus)
      this.input.addEventListener("keydown", this.onKeyDown)
      this.el.addEventListener("focusout", this.onFocusOut)
      this.bindOptions()
      this.bindClear()
      document.addEventListener("pointerdown", this.onDocumentPointerDown)

      this.applyFilter()
    },

    updated() {
      this.bindOptions()
      this.bindClear()
      if (!this.open) this.restore()
      else this.setEditing(this.editing)
      this.setOpen(this.open)
      this.applyFilter()
    },

    destroyed() {
      this.input.removeEventListener("input", this.onInput)
      this.input.removeEventListener("change", this.onChange)
      this.input.removeEventListener("focus", this.onFocus)
      this.input.removeEventListener("keydown", this.onKeyDown)
      this.el.removeEventListener("focusout", this.onFocusOut)
      this.options().forEach(option => option.removeEventListener("mousedown", this.onOptionMouseDown))
      this.clearButton?.removeEventListener("click", this.onClear)
      this.clearButton?.removeEventListener("mousedown", this.onClearMouseDown)
      document.removeEventListener("pointerdown", this.onDocumentPointerDown)
    },
  }

  // Companion to the reveal option of a password input.
  //
  // The input's `type` is the only record of masked-or-shown: the click flips
  // it, and everything else is derived from it -- the button's accessible
  // name, its title and which glyph shows -- by one read of the attribute, so
  // however many clicks land the state cannot drift, and a stored secret
  // that sets `type` itself (SecretStored) is reflected the same way.
  //
  // A pointer press on the toggle must not pull focus out of the input, and
  // the caret must survive the type swap (browsers reset the selection when
  // `type` changes), so the user can look, then keep typing where they were.
  // Keyboard activation still focuses the button as usual.
  const SecretReveal = {
    mounted() {
      this.input = document.getElementById(this.el.getAttribute("aria-controls"))
      this.showGlyph = document.getElementById(`${this.el.id}-show`)
      this.hideGlyph = document.getElementById(`${this.el.id}-hide`)

      this.sync = () => {
        const shown = this.input.getAttribute("type") === "text"
        const labels = this.el.dataset

        this.el.setAttribute("aria-label", shown ? labels.hideLabel : labels.showLabel)
        this.el.setAttribute("title", shown ? labels.hideTitle : labels.showTitle)
        this.showGlyph.classList.toggle("hidden", shown)
        this.hideGlyph.classList.toggle("hidden", !shown)
      }

      this.onMouseDown = event => event.preventDefault()

      this.onClick = () => {
        const focused = document.activeElement === this.input
        const {selectionStart, selectionEnd} = this.input
        this.input.setAttribute("type", this.input.getAttribute("type") === "text" ? "password" : "text")
        if (focused) this.input.setSelectionRange(selectionStart, selectionEnd)
      }

      this.el.addEventListener("mousedown", this.onMouseDown)
      this.el.addEventListener("click", this.onClick)

      this.masking = new MutationObserver(this.sync)
      this.masking.observe(this.input, {attributeFilter: ["type"]})

      this.sync()
    },

    updated() {
      this.sync()
    },

    destroyed() {
      this.masking.disconnect()
      this.el.removeEventListener("mousedown", this.onMouseDown)
      this.el.removeEventListener("click", this.onClick)
    },
  }

  // Clear a stored secret's keep-current mask in the input property that
  // forms submit. Changing the value attribute would leave the submitted
  // value intact.
  const SecretClear = {
    mounted() {
      this.input = document.getElementById(this.el.dataset.inputId)
      this.clear = () => {
        this.input.value = ""
        this.input.dispatchEvent(new Event("secret:cleared"))
        this.input.focus()
      }
      this.el.addEventListener("click", this.clear)
    },

    destroyed() {
      this.el.removeEventListener("click", this.clear)
    },
  }

  // Only a successful server-side reauthentication sends plaintext to this
  // hook, as a `bilibi:secret:reveal` window event ({id, value, duration_ms})
  // dispatched by the transport. No plaintext
  // is rendered in initial HTML or kept in a hook field. The input property
  // is reset locally even if the connection drops before the timeout. A value
  // the user types or clears during the window is kept, never re-masked.
  const SecretStored = {
    mounted() {
      this.mask = this.el.value
      this.stop = () => {
        clearTimeout(this.timer)
        this.revealing = false
        this.el.type = "password"
      }
      this.onReveal = ({detail}) => {
        const {id, value, duration_ms} = detail || {}
        if (id !== this.el.id) return
        clearTimeout(this.timer)
        this.el.value = value
        this.el.type = "text"
        this.revealing = true
        this.timer = setTimeout(() => this.hide(), duration_ms)
      }
      this.el.addEventListener("secret:cleared", this.stop)
      this.el.addEventListener("input", this.stop)
      window.addEventListener("bilibi:secret:reveal", this.onReveal)
    },

    hide() {
      if (this.revealing) this.el.value = this.mask
      this.stop()
    },

    destroyed() {
      this.hide()
      this.el.removeEventListener("secret:cleared", this.stop)
      this.el.removeEventListener("input", this.stop)
      window.removeEventListener("bilibi:secret:reveal", this.onReveal)
    },
  }

  // Companion to the multi-select and the record history disclosure. The
  // first `[aria-expanded]` inside the wrapper is the trigger whose state is
  // the single record of open; the list and the chevron derive from it in
  // CSS, so they cannot disagree with it. This hook writes that one attribute
  // on every path: the trigger's click toggles it, and a click outside the
  // field, focus leaving the field, and Escape close it.
  //
  // Escape listens on `window`, not the field: Safari and macOS Firefox blur
  // without focusing the button being pressed, so a list opened by mouse
  // there is open while focus sits on `body`, and a listener on the field
  // would never see the key. It is attached only while the list is open, so
  // no closed field is watching keys and no other Escape handler on the page
  // loses one. Reaching that far means the key also arrives from a search
  // box the user is typing in, so only a press with focus already inside the
  // field returns focus to the trigger; everywhere else the list closes and
  // the caret stays where the user put it.
  //
  // A pointer press starting inside the field is not focus leaving it, even
  // when the browser reports no new target: the click that press belongs to
  // already owns the toggle, so dismissing on the blur would close the list
  // and let the same click reopen it. The press ends on the release, which
  // arrives even when the click never does.
  //
  // A wrapper marked `data-keep-on-blur` is a bar a person works in rather
  // than a list they pick from: it closes on its own control and on Escape,
  // and on a click outside it only if it also carries `data-click-away`.
  const DisclosureDismiss = {
    mounted() {
      this.pressing = false
      this.listening = false
      this.trigger = this.el.querySelector("[aria-expanded]")
      this.keepOnBlur = this.el.hasAttribute("data-keep-on-blur")
      this.clickAway = !this.keepOnBlur || this.el.hasAttribute("data-click-away")

      this.isOpen = () => this.trigger.getAttribute("aria-expanded") === "true"
      this.dismiss = () => this.trigger.setAttribute("aria-expanded", "false")
      this.escape = () => {
        this.dismiss()
        this.trigger.focus()
      }
      this.toggle = () => this.trigger.setAttribute("aria-expanded", this.isOpen() ? "false" : "true")

      this.onPointerDown = () => (this.pressing = true)
      this.onPressEnd = () => (this.pressing = false)

      this.onKeyDown = e => {
        if (e.key !== "Escape") return

        if (this.el.contains(document.activeElement)) this.escape()
        else this.dismiss()
      }

      this.onClickAway = e => {
        if (!this.el.contains(e.target)) this.dismiss()
      }

      this.onFocusOut = e => {
        if (this.keepOnBlur) return

        if (this.pressing) {
          this.pressing = false
          return
        }

        if (e.relatedTarget && this.el.contains(e.relatedTarget)) return

        this.dismiss()
      }

      this.syncListeners = () => {
        const open = this.isOpen()
        if (open === this.listening) return

        this.listening = open

        if (open) {
          window.addEventListener("keydown", this.onKeyDown)
          if (this.clickAway) document.addEventListener("click", this.onClickAway)
        } else {
          window.removeEventListener("keydown", this.onKeyDown)
          document.removeEventListener("click", this.onClickAway)
        }
      }

      this.expansion = new MutationObserver(this.syncListeners)
      this.expansion.observe(this.trigger, {attributeFilter: ["aria-expanded"]})

      this.trigger.addEventListener("click", this.toggle)
      this.el.addEventListener("pointerdown", this.onPointerDown)
      this.el.addEventListener("focusout", this.onFocusOut)
      window.addEventListener("pointerup", this.onPressEnd)
      window.addEventListener("pointercancel", this.onPressEnd)

      this.syncListeners()
    },

    destroyed() {
      this.expansion.disconnect()
      this.trigger.removeEventListener("click", this.toggle)
      this.el.removeEventListener("pointerdown", this.onPointerDown)
      this.el.removeEventListener("focusout", this.onFocusOut)
      window.removeEventListener("keydown", this.onKeyDown)
      document.removeEventListener("click", this.onClickAway)
      window.removeEventListener("pointerup", this.onPressEnd)
      window.removeEventListener("pointercancel", this.onPressEnd)
    },
  }

  // multi_select disclosure with Bilibi's inset chip presentation.
  // Values remain the owner's; overflow is measured from the actual field width.
  const MultiSelect = {
    mounted() {
      DisclosureDismiss.mounted.call(this)
      this.paintChips = () => {
        const row = this.el.querySelector("[data-chip-row]")
        if (!row || !row.clientWidth) return
        const chips = [...row.querySelectorAll("[data-selected-chip]")]
        const marker = row.querySelector("[data-chip-overflow]")
        const gap = parseFloat(getComputedStyle(row).columnGap) || 0
        const widths = chips.map(chip => chip.getBoundingClientRect().width)
        const total = widths.reduce((sum, width) => sum + width, 0) + Math.max(0, chips.length - 1) * gap
        let visible = chips.length
        if (!this.el.hasAttribute("data-wrap-chips") && total > row.clientWidth) {
          const budget = row.clientWidth - parseFloat(getComputedStyle(marker).width) - gap
          let used = 0
          visible = 0
          for (const width of widths) {
            const next = used + (visible ? gap : 0) + width
            if (next > budget) break
            used = next
            visible++
          }
        }
        chips.forEach((chip, index) => {
          const hidden = index >= visible
          if (chip.hasAttribute("data-overflow") !== hidden) chip.toggleAttribute("data-overflow", hidden)
          if (chip.getAttribute("aria-hidden") !== String(hidden)) chip.setAttribute("aria-hidden", String(hidden))
        })
        const hidden = visible === chips.length
        if (marker.hidden !== hidden) marker.hidden = hidden
        const title = hidden ? "" : `${chips.length - visible} more selected`
        if (marker.title !== title) marker.title = title
      }
      this.scheduleChips = () => {
        if (this.chipFrame) return
        this.chipFrame = requestAnimationFrame(() => {
          this.chipFrame = null
          this.paintChips()
        })
      }
      this.removeChip = event => {
        const button = event.target.closest(".ui-multi-remove")
        if (!button || button.disabled) return
        if (!button.hasAttribute("data-remove-value")) {
          this.trigger.focus()
          return
        }
        const input = [...this.el.querySelectorAll('input[type="checkbox"]')]
          .find(option => option.value === button.dataset.removeValue)
        if (!input || input.disabled) return
        input.checked = false
        input.dispatchEvent(new Event("change", {bubbles: true}))
        this.trigger.focus()
      }
      this.el.addEventListener("click", this.removeChip)
      this.chipResize = new ResizeObserver(this.scheduleChips)
      this.chipResize.observe(this.el)
      this.scheduleChips()
    },
    updated() { this.scheduleChips?.() },
    destroyed() {
      DisclosureDismiss.destroyed.call(this)
      this.chipResize.disconnect()
      cancelAnimationFrame(this.chipFrame)
      this.el.removeEventListener("click", this.removeChip)
    },
  }

  // Server-owned search and selection; results are never filtered in the browser.
  const SearchMultiSelect = {
    mounted() {
      this.input = this.el.querySelector('[role="combobox"]')
      this.list = this.el.querySelector('[role="listbox"]')
      this.popup = this.el.querySelector('[data-search-popup]')
      this.open = false
      this.active = -1
      this.current = () => this.el.dataset.query.trim() === this.input.value.trim()
      this.options = () => [...this.list.querySelectorAll('[role="option"]')]
        .filter(option => !option.hidden && !option.parentElement.hidden && (option.hasAttribute("data-create") || this.current()))
      this.paint = () => {
        this.input.setAttribute("aria-expanded", String(this.open))
        this.popup.hidden = !this.open
        const create = this.list.querySelector('[data-create]')
        if (create) {
          const query = this.input.value.trim()
          create.hidden = !query
          const label = `${this.el.dataset.createLabel} ${query}`
          if (create.textContent !== label) create.textContent = label
        }
        this.popup.toggleAttribute('data-stale', !this.current())
        const options = this.options()
        const status = this.el.querySelector('[data-result-status]')
        const count = options.filter(option => !option.hasAttribute('data-create')).length
        const selection = this.el.querySelector('.ui-multi-heading span').textContent
        const message = this.current() ? `${count} ${count === 1 ? 'result' : 'results'} available, ${selection}` : `Searching, ${selection}`
        if (status.textContent !== message) status.textContent = message
        if (this.active >= options.length) this.active = -1
        this.list.querySelectorAll('[role="option"]').forEach(option => {
          option.classList.toggle("bg-brand-surface", option === options[this.active])
        })
        const option = options[this.active]
        if (option && this.open) this.input.setAttribute("aria-activedescendant", option.id)
        else this.input.removeAttribute("aria-activedescendant")
      }
      this.choose = option => {
        if (!option || option.getAttribute("aria-disabled") === "true") return
        if (option.hasAttribute("data-create")) this.emit(this.el.dataset.createEvent, {query: this.input.value.trim()})
        else this.emit(this.el.dataset.pickEvent, {value: option.dataset.value})
        this.active = -1
        this.input.focus()
        this.paint()
      }
      this.onInput = () => {
        this.open = true
        this.active = -1
        this.paint()
        clearTimeout(this.timer)
        const query = this.input.value
        this.timer = setTimeout(() => this.emit(this.el.dataset.searchEvent, {query}), 300)
      }
      this.onFocus = () => { this.open = true; this.paint() }
      this.onKey = event => {
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          event.preventDefault()
          this.open = true
          this.popup.hidden = false
          const options = this.options()
          const delta = event.key === "ArrowDown" ? 1 : -1
          this.active = options.length ? (this.active < 0 ? (delta > 0 ? 0 : options.length - 1) : (this.active + delta + options.length) % options.length) : -1
          this.paint()
          options[this.active]?.scrollIntoView({block: "nearest"})
        } else if (event.key === "Enter" && this.open) {
          event.preventDefault()
          if (this.active >= 0) this.choose(this.options()[this.active])
        } else if (event.key === "Escape" && this.open) {
          event.preventDefault()
          event.stopPropagation()
          this.open = false
          this.active = -1
          this.paint()
        }
      }
      this.onClick = event => {
        if (event.target.closest('[data-chip-overflow]')) {
          this.expandedChips = true
          this.el.setAttribute('data-expanded-chips', '')
          this.scheduleChips()
          this.input.focus()
          return
        }
        const remove = event.target.closest('[data-remove]')
        if (remove) { this.emit(this.el.dataset.removeEvent, {value: remove.dataset.remove}); this.input.focus(); return }
        const option = event.target.closest('[role="option"]')
        if (option) this.choose(option)
      }
      this.onPointer = event => {
        if (event.target.closest('[role="option"]')) event.preventDefault()
        else if (!event.target.closest('input, button, a') && event.target.closest('.ui-multi-field')) this.input.focus()
      }
      this.onFocusOut = () => requestAnimationFrame(() => {
        if (!this.el.contains(document.activeElement)) { this.open = false; this.active = -1; this.paint() }
      })
      this.onChange = event => event.stopPropagation()
      this.input.addEventListener("input", this.onInput)
      this.input.addEventListener("change", this.onChange)
      this.input.addEventListener("focus", this.onFocus)
      this.input.addEventListener("keydown", this.onKey)
      this.el.addEventListener("click", this.onClick)
      this.el.addEventListener("pointerdown", this.onPointer)
      this.el.addEventListener("focusout", this.onFocusOut)
      this.paintChips = () => {
        if (this.expandedChips && !this.el.hasAttribute('data-expanded-chips')) this.el.setAttribute('data-expanded-chips', '')
        const row = this.el.querySelector('.ui-multi-chips')
        const chips = [...row.querySelectorAll('[data-selected-chip]')]
        const marker = row.querySelector('[data-chip-overflow]')
        const gap = parseFloat(getComputedStyle(row).columnGap) || 0
        const budget = row.clientWidth - 96 - 16 - gap * 2
        let used = 0
        let visible = 0
        for (const chip of chips) {
          const width = chip.getBoundingClientRect().width
          const fits = this.expandedChips || this.el.hasAttribute('data-wrap-chips') || used + width + (visible ? gap : 0) <= budget
          if (fits) { used += width + (visible ? gap : 0); visible++ }
          chip.toggleAttribute('data-overflow', !fits)
          chip.setAttribute('aria-hidden', String(!fits))
        }
        marker.hidden = visible === chips.length
        marker.title = marker.hidden ? '' : `${chips.length - visible} more selected`
        if (!marker.hidden) marker.setAttribute('aria-label', `Show ${chips.length - visible} more selected`)
      }
      this.scheduleChips = () => {
        if (this.frame) return
        this.frame = requestAnimationFrame(() => { this.frame = null; this.paintChips() })
      }
      this.resize = new ResizeObserver(this.scheduleChips)
      this.resize.observe(this.el)
      this.paint()
      this.scheduleChips()
    },
    updated() { this.paint?.(); this.scheduleChips?.() },
    destroyed() {
      this.resize.disconnect()
      cancelAnimationFrame(this.frame)
      clearTimeout(this.timer)
      this.input.removeEventListener("input", this.onInput)
      this.input.removeEventListener("change", this.onChange)
      this.input.removeEventListener("focus", this.onFocus)
      this.input.removeEventListener("keydown", this.onKey)
      this.el.removeEventListener("click", this.onClick)
      this.el.removeEventListener("pointerdown", this.onPointer)
      this.el.removeEventListener("focusout", this.onFocusOut)
    },
  }

  // Drag-to-reorder for the dashboard widget grid.
  // Server-driven: the hook only computes the DOM order after a drop and
  // reports it as `reorder_widgets`; the dashboard validates the order is a
  // permutation of its current widget ids before persisting anything, so a
  // stale or forged report changes nothing. Dragging starts only from a card's
  // drag handle (the one draggable control); HTML5 drag and drop has no touch
  // support, so the move buttons remain the accessible reorder path.
  const DashboardSort = {
    mounted() {
      this.dragged = null
      this.onDragStart = event => {
        const item = event.target.closest("[data-widget-id]")
        if (!item || !this.el.contains(item)) return
        this.dragged = item
        event.dataTransfer.effectAllowed = "move"
        event.dataTransfer.setData("text/plain", item.dataset.widgetId)
        item.classList.add("opacity-40")
      }
      this.onDragOver = event => {
        if (!this.dragged) return
        event.preventDefault()
        event.dataTransfer.dropEffect = "move"
        const target = event.target.closest("[data-widget-id]")
        if (!target || target === this.dragged || !this.el.contains(target)) return
        // Before or after the hovered card by the pointer's half of it, so the
        // card follows the cursor the way people expect.
        const rect = target.getBoundingClientRect()
        const before = event.clientY < rect.top + rect.height / 2
        target.parentNode.insertBefore(this.dragged, before ? target : target.nextSibling)
      }
      this.onDrop = event => {
        if (this.dragged) event.preventDefault()
      }
      this.onDragEnd = () => {
        if (!this.dragged) return
        this.dragged.classList.remove("opacity-40")
        this.dragged = null
        this.emit("reorder_widgets", {ids: [...this.el.querySelectorAll("[data-widget-id]")].map(item => item.dataset.widgetId)})
      }
      this.el.addEventListener("dragstart", this.onDragStart)
      this.el.addEventListener("dragover", this.onDragOver)
      this.el.addEventListener("drop", this.onDrop)
      this.el.addEventListener("dragend", this.onDragEnd)
    },

    destroyed() {
      this.el.removeEventListener("dragstart", this.onDragStart)
      this.el.removeEventListener("dragover", this.onDragOver)
      this.el.removeEventListener("drop", this.onDrop)
      this.el.removeEventListener("dragend", this.onDragEnd)
    },
  }

  // A Fold (components.html `fold`) is a native details whose open state the
  // person owns; the patcher leaves it alone. The server marks data-open when
  // the fold must show (an error inside it), and this opens it on that change
  // only, so closing it again by hand sticks until the next new reason.
  const Fold = {
    mounted() {
      this.forced = this.el.hasAttribute("data-open")
    },

    updated() {
      const forced = this.el.hasAttribute("data-open")
      if (forced && !this.forced && !this.el.open) this.el.open = true
      this.forced = forced
    },
  }

  // The browser half of forms.html `location_action`. The wrapper carries the event, its
  // `data-value-*` values (live.js's spelling for a button's values), the
  // working labels, the timeout and the server's `data-disabled`; the shared
  // button inside it is what the person clicks. A click puts the button in the
  // transport's busy shape (disabled, aria-busy and data-live-busy, so a patch
  // leaves its text and state alone) with the `working` label while the
  // browser locates, swaps in `sending` once the event is raised and settles
  // it when the page answers, taking the server's disabled state from the
  // wrapper. The busy spell is kept here, not in attributes: a patch makes a
  // busy control's data-* attributes follow the server (the reply's patch
  // strips the marker before onReply runs), so updated() re-holds the shape,
  // writing only what differs. One event per click: the values plus latitude,
  // longitude and accuracy, or the values plus outcome permission_denied (the
  // person or the browser refused), unavailable (no geolocation, no fix in
  // time, or no numbers) or insecure_context (an http page off localhost,
  // where the browser offers no geolocation). A position that arrives after
  // the control left the page is dropped; one raised while the socket is
  // down is queued by live.js.
  const LocationAction = {
    mounted() {
      this.generation = 0
      this.pending = null
      this.onClick = event => {
        const button = event.target.closest("button")
        if (!button || button.disabled || this.pending || !this.el.contains(button)) return
        const generation = ++this.generation
        const current = () => this.el.isConnected && this.generation === generation
        this.pending = {button, text: button.textContent, label: this.el.dataset.working || button.textContent}
        this.hold()
        const raise = values => {
          if (!current()) return
          if (this.el.dataset.sending) {
            this.pending.label = this.el.dataset.sending
            this.hold()
          }
          this.emit(this.el.dataset.event, {...this.values(), ...values, onReply: () => {
            if (current()) this.settle()
          }})
        }
        const failed = outcome => raise({outcome})
        if (!window.isSecureContext) return failed("insecure_context")
        if (!navigator.geolocation) return failed("unavailable")
        try {
          navigator.geolocation.getCurrentPosition(position => {
            const {latitude, longitude, accuracy} = position.coords || {}
            if (!Number.isFinite(latitude) || !Number.isFinite(longitude)) return failed("unavailable")
            raise(Number.isFinite(accuracy) ? {latitude, longitude, accuracy} : {latitude, longitude})
          }, error => failed(error && error.code === 1 ? "permission_denied" : "unavailable"),
          {enableHighAccuracy: true, maximumAge: 0, timeout: Number(this.el.dataset.timeout) || 15000})
        } catch (_error) {
          failed("unavailable")
        }
      }
      this.el.addEventListener("click", this.onClick)
    },

    // data-value-clock_type="in" is {clock_type: "in"}, as live.js reads a button.
    values() {
      const values = {}
      for (const [key, value] of Object.entries(this.el.dataset)) {
        if (key.startsWith("value") && key.length > 5) values[key[5].toLowerCase() + key.slice(6)] = value
      }
      return values
    },

    hold() {
      const {button, label} = this.pending
      if (button.textContent !== label) button.textContent = label
      if (!button.disabled) button.disabled = true
      if (button.getAttribute("aria-busy") !== "true") button.setAttribute("aria-busy", "true")
      if (!button.hasAttribute("data-live-busy")) button.setAttribute("data-live-busy", "")
    },

    updated() {
      if (this.pending) this.hold()
    },

    settle() {
      const {button, text} = this.pending
      this.pending = null
      button.removeAttribute("data-live-busy")
      button.removeAttribute("aria-busy")
      button.textContent = text
      button.disabled = this.el.hasAttribute("data-disabled")
    },

    destroyed() {
      this.generation++
      this.pending = null
      this.el.removeEventListener("click", this.onClick)
    },
  }

  const registry = window.BilibiUI
  if (!registry) throw new Error("hooks.js needs ui.js loaded first")
  for (const [name, hook] of Object.entries({FactorProof, CodeInput, ColourInput, Combobox, SecretReveal, SecretClear, SecretStored, DisclosureDismiss, MultiSelect, SearchMultiSelect, DashboardSort, Fold, LocationAction})) {
    registry.registerHook(name, hook)
  }
})()

;(() => {
  // Graphic's catalogue reports the actual clipboard result. Never claim a
  // successful copy merely because the tile was clicked.
  class ClipboardCopy {
    mounted() {
      this.onClick = async (event) => {
        const tile = event.target.closest('[data-copy-text]');
        if (!tile || !this.el.contains(tile)) return;
        const text = tile.dataset.copyText;
        let copied = false;
        try { await navigator.clipboard.writeText(text); copied = true; } catch { /* a refusal changes nothing */ }
        // A `copy_button` tile answers for itself: a check named "Copied" for
        // a moment, and nothing at all when the browser refuses. Only the
        // catalogue's tiles report to the server.
        const button = tile.querySelector('[data-copy-button]');
        if (!button) return this.emit(this.el.dataset.copyEvent || 'icon-copied', {text, copied});
        if (!copied) return;
        tile.setAttribute('data-copied', '');
        button.setAttribute('aria-label', 'Copied');
        button.setAttribute('title', 'Copied');
        clearTimeout(tile.copyTimer);
        tile.copyTimer = setTimeout(() => {
          tile.removeAttribute('data-copied');
          button.setAttribute('aria-label', button.dataset.copyLabel);
          button.setAttribute('title', button.dataset.copyLabel);
        }, 1500);
      };
      this.el.addEventListener('click', this.onClick);
    }
    destroyed() { this.el.removeEventListener('click', this.onClick); }
  }
  window.BilibiUI.registerHook('ClipboardCopy', ClipboardCopy.prototype);
})();

;(() => {
  // DateInput (the date, date-and-time and time kinds of forms::input, and
  // forms::date_box): one text box that reads and shows the shared formats,
  // "6 Nov 2026", "6 Nov 2026, 14:30" and "14:30", on every browser. A native
  // date or time input draws the browser's own order and clock (mm/dd/yyyy,
  // 02:30 PM on an American system), so it is never the visible box here.
  // The box has no name; its hidden sibling posts what the server always
  // read (yyyy-mm-dd, yyyy-mm-ddTHH:MM or HH:MM), kept in step on every
  // keystroke, so a change, Enter or a submit button carries what the box
  // shows. A date may be typed "6 Nov 2026", "6/11/2026" (day first),
  // "2026-11-06" or "Nov 6 2026"; a time "14:30", "2:30 pm" or "1430". Text
  // it cannot read is posted as typed, so the server's own validation
  // answers it. The icon button opens the browser's picker on a hidden
  // native input (showPicker), which carries min and max.
  const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
  const monthOf = (word) => {
    const key = word.toLowerCase()
    if (key.length < 3) return 0
    return MONTHS.findIndex((month) => key.startsWith(month.toLowerCase())) + 1
  }
  const pad = (n) => String(n).padStart(2, "0")
  const isoDate = (y, m, d) => {
    if (y < 100) y += 2000
    const date = new Date(Date.UTC(y, m - 1, d))
    if (date.getUTCFullYear() !== y || date.getUTCMonth() !== m - 1 || date.getUTCDate() !== d) return null
    return `${y}-${pad(m)}-${pad(d)}`
  }
  const parseDate = (text) => {
    const value = text.trim()
    let match
    if ((match = /^(\d{4})-(\d{1,2})-(\d{1,2})$/.exec(value))) return isoDate(+match[1], +match[2], +match[3])
    if ((match = /^(\d{1,2})[/.\-\s](\d{1,2})[/.\-\s](\d{2}|\d{4})$/.exec(value))) return isoDate(+match[3], +match[2], +match[1])
    const words = value.replace(/,/g, " ").split(/[\s/.\-]+/).filter(Boolean)
    if (words.length !== 3 || !/^\d{2}(\d{2})?$/.test(words[2])) return null
    const year = +words[2]
    if (/^\d{1,2}$/.test(words[0]) && monthOf(words[1])) return isoDate(year, monthOf(words[1]), +words[0])
    if (monthOf(words[0]) && /^\d{1,2}$/.test(words[1])) return isoDate(year, monthOf(words[0]), +words[1])
    return null
  }
  const parseTime = (text) => {
    const match = /^(\d{1,2})(?:[:.h]?(\d{2}))?(?::\d{2})?\s*(?:([ap])\.?m?\.?)?$/i.exec(text.trim())
    if (!match) return null
    let hour = +match[1]
    const minute = match[2] === undefined ? 0 : +match[2]
    const meridiem = match[3]?.toLowerCase()
    if (meridiem) {
      if (hour < 1 || hour > 12) return null
      hour = (hour % 12) + (meridiem === "p" ? 12 : 0)
    }
    if (hour > 23 || minute > 59) return null
    return `${pad(hour)}:${pad(minute)}`
  }
  // The value the server reads for the box's text, or null.
  const parse = (kind, text) => {
    if (kind === "time") return parseTime(text)
    if (kind !== "datetime-local") return parseDate(text)
    const value = text.trim()
    const split = /^(\d{4}-\d{1,2}-\d{1,2})[T\s]+(.+)$/.exec(value) || /^(.*\d{4})[,\s]+(.+)$/.exec(value)
    if (!split) return null
    const date = parseDate(split[1]), time = parseTime(split[2])
    return date && time ? `${date}T${time}` : null
  }
  // The box's text for a posted value, in the shared format.
  const show = (kind, value) => {
    if (!value) return ""
    if (kind === "time") return /^\d{2}:\d{2}/.test(value) ? value.slice(0, 5) : value
    const match = /^(\d{4})-(\d{2})-(\d{2})(?:T(\d{2}:\d{2}))?/.exec(value)
    if (!match) return value
    const date = `${+match[3]} ${MONTHS[+match[2] - 1]} ${match[1]}`
    return kind === "datetime-local" && match[4] ? `${date}, ${match[4]}` : date
  }

  const syncPicker = (picker, parsed) => {
    if (!picker) return
    const inside = parsed && (!picker.min || parsed >= picker.min) && (!picker.max || parsed <= picker.max)
    picker.value = inside ? parsed : ""
  }

  window.BilibiUI.registerHook("DateInput", {
    mounted() {
      this.kind = () => this.el.dataset.dateKind || "date"
      this.box = () => this.el.querySelector('input[type="text"]')
      this.value = () => this.el.querySelector("input[data-date-value]")
      this.picker = () => this.el.querySelector("input[data-date-picker]")
      // Runs on the wrapper, before live.js reads the form at the document,
      // so the posted value always matches the box.
      this.onInput = (event) => {
        const box = this.box(), value = this.value(), picker = this.picker(), kind = this.kind()
        if (!box || !value) return
        if (event.target === picker) {
          if (!picker.value) return
          value.value = picker.value
          box.value = show(kind, picker.value)
          return
        }
        if (event.target !== box) return
        const parsed = parse(kind, box.value)
        value.value = parsed ?? box.value.trim()
        syncPicker(picker, parsed)
        // On leaving the box (change), show what was understood.
        if (event.type === "change" && parsed) box.value = show(kind, parsed)
      }
      this.onClick = (event) => {
        const button = event.target instanceof Element ? event.target.closest("button") : null
        const picker = this.picker()
        if (!button || button.disabled || !picker) return
        event.preventDefault()
        try {
          picker.showPicker()
        } catch {
          // A browser without showPicker still has the typed box.
          this.box()?.focus()
        }
      }
      // An inline editor opening on a value (or on the first key typed)
      // hands it here, so the box shows it in the shared format.
      this.onValue = (event) => {
        const box = this.box(), value = this.value(), picker = this.picker(), given = event.detail?.value ?? ""
        if (!box || !value) return
        const parsed = parse(this.kind(), given) ?? (/^\d{4}-\d{2}-\d{2}/.test(given) ? given : null)
        value.value = parsed ?? given
        syncPicker(picker, parsed)
        box.value = show(this.kind(), parsed ?? given)
      }
      this.el.addEventListener("input", this.onInput)
      this.el.addEventListener("change", this.onInput)
      this.el.addEventListener("click", this.onClick)
      this.el.addEventListener("bilibi:date-value", this.onValue)
    },
    destroyed() {
      this.el.removeEventListener("input", this.onInput)
      this.el.removeEventListener("change", this.onInput)
      this.el.removeEventListener("click", this.onClick)
      this.el.removeEventListener("bilibi:date-value", this.onValue)
    },
  })
})();

;(() => {
  // FilterSheet (the filter toolbar of templates/lists.html): below `sm` the
  // filters other than search sit in a <dialog> sheet. This hook only opens
  // and closes it and resets a filter whose chip is removed; the filters
  // themselves stay ordinary form controls, so a change posts with the form
  // and the server's reply re-renders the count and the chips. The patcher
  // keeps the dialog's `open`, so the sheet stays up while filters apply.
  const WIDE = window.matchMedia("(min-width: 40rem)")

  // A removed chip puts its filter back to the first choice (a select's
  // "All"), unticks a multi-select and empties a date, then raises one
  // change, which live.js turns into the toolbar's event.
  const reset = (control) => {
    let changed = null
    for (const el of control.querySelectorAll("select, input")) {
      if (el.disabled) continue
      if (el.tagName === "SELECT") {
        const first = el.options[0]?.value ?? ""
        if (el.value !== first) { el.value = first; changed = el }
      } else if (el.type === "checkbox" || el.type === "radio") {
        if (el.checked) { el.checked = false; changed = el }
      } else if (el.type !== "hidden" || el.name) {
        if (el.value !== "") { el.value = ""; changed = el }
      }
    }
    changed?.dispatchEvent(new Event("change", {bubbles: true}))
  }

  window.BilibiUI.registerHook("FilterSheet", {
    mounted() {
      const id = this.el.id
      this.sheet = () => this.el.querySelector(`#${CSS.escape(id)}-filters`)
      this.onClick = (event) => {
        const sheet = this.sheet()
        if (!sheet) return
        const button = event.target instanceof Element ? event.target.closest("button") : null
        if (event.target === sheet && sheet.open) {
          // A click on the backdrop lands on the dialog itself.
          const box = sheet.getBoundingClientRect()
          const inside = event.clientX >= box.left && event.clientX <= box.right && event.clientY >= box.top && event.clientY <= box.bottom
          if (!inside) sheet.close()
          return
        }
        if (!button) return
        if (button.id === `${id}-filters-open`) {
          if (!sheet.open) sheet.showModal()
        } else if (button.id === `${id}-filters-close` || button.id === `${id}-filters-done`) {
          sheet.close()
        } else if (button.id.startsWith(`${id}-chip-`) && button.id.endsWith("-remove")) {
          const control = this.el.querySelector(`[data-filter-control="${CSS.escape(button.value)}"]`)
          if (control) reset(control)
        }
      }
      // Widening past `sm` draws the filters in the row again, so a sheet
      // left open would hold the top layer with nothing to show.
      this.onWide = () => {
        const sheet = this.sheet()
        if (WIDE.matches && sheet?.open) sheet.close()
      }
      this.el.addEventListener("click", this.onClick)
      WIDE.addEventListener("change", this.onWide)
    },
    destroyed() {
      this.el.removeEventListener("click", this.onClick)
      WIDE.removeEventListener("change", this.onWide)
    },
  })
})()
