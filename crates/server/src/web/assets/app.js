"use strict";

/*
 * Progressive enhancement for the Plamenu web UI.
 *
 * Contract: the server-rendered HTML already works on its own — links
 * navigate, forms POST, the action buttons toggle via PRG. Everything here is
 * additive. If this script never runs, nothing is lost; it only removes full
 * page reloads and refreshes timestamps in place.
 */
(() => {
  // ---- In-place action toggles (favourite / boost / bookmark / votes) --
  //
  // Each toggle is a real <form> that POSTs to /web/statuses/{id}/{verb}. We
  // intercept the submit, fire the same request in the background (without
  // following the 303 back), and flip every copy of the button optimistically.
  // A status may appear more than once on one page (for example as a post and
  // inside a thread group), so treating the status id as the synchronization
  // key keeps all copies honest. Group votes need one extra wrinkle: up/down
  // are mutually exclusive and their shared score lives between the buttons.
  function bindActionForms(root) {
    root.querySelectorAll("form.action-form[data-action]").forEach((form) => {
      if (form.dataset.bound) return;
      form.dataset.bound = "1";
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        toggleAction(form);
      });
    });
  }

  async function toggleAction(form) {
    const button = form.querySelector("button");
    const base = form.dataset.action; // favourite | reblog | bookmark | up/downvote
    const wasActive = button.classList.contains("is-active");
    const id = statusActionParts(form)?.id;
    if (!id) {
      form.submit();
      return;
    }

    const voteAction = base === "upvote" || base === "downvote";
    const actionForms = statusActionForms(id).filter((other) =>
      voteAction
        ? other.dataset.action === "upvote" || other.dataset.action === "downvote"
        : other.dataset.action === base
    );
    const scores = voteAction ? statusVoteScores(id) : [];
    const snapshot = actionForms.map(snapshotActionForm);
    const scoreSnapshot = [...scores].map((score) => [score, score.textContent]);
    const requestAction = `/web/statuses/${id}/${!wasActive ? base : "un" + base}`;

    if (voteAction) {
      updateVoteState(id, base, wasActive, scores);
    } else {
      actionForms
        .filter((other) => other.dataset.action === base)
        .forEach((other) => {
          setActionFormActive(other, !wasActive);
          bumpCount(other.querySelector("button"), wasActive ? -1 : 1);
        });
    }
    setActionPending(actionForms, true, base);

    try {
      const body = new URLSearchParams(new FormData(form));
      const res = await fetch(requestAction, {
        method: "POST",
        body,
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        redirect: "manual",
        credentials: "same-origin",
      });
      // `redirect: manual` reports a 303 as an opaque redirect (status 0).
      if (res.status !== 0 && !res.ok) throw new Error(res.statusText);
    } catch (_err) {
      snapshot.forEach(restoreActionForm);
      scoreSnapshot.forEach(([score, text]) => { score.textContent = text; });
      return;
    }
    setActionPending(actionForms, false, base);
  }

  function statusActionParts(form) {
    try {
      const path = new URL(form.getAttribute("action"), window.location.origin).pathname;
      const match = path.match(/^\/web\/statuses\/(\d+)\/(?:un)?([^/]+)$/);
      return match ? { id: match[1], verb: match[2] } : null;
    } catch (_err) {
      return null;
    }
  }

  function statusActionForms(id) {
    return [...document.querySelectorAll("form.action-form[data-action]")].filter(
      (form) => statusActionParts(form)?.id === id
    );
  }

  function statusVoteScores(id) {
    const scores = [];
    statusActionForms(id)
      .filter((form) => form.dataset.action === "upvote")
      .forEach((form) => {
        const score = form.closest(".status__votes")?.querySelector(".status__score");
        if (score && !scores.includes(score)) scores.push(score);
      });
    return scores;
  }

  function snapshotActionForm(form) {
    const button = form.querySelector("button");
    const count = button?.querySelector(".action__count");
    return {
      form,
      action: form.getAttribute("action"),
      active: button?.classList.contains("is-active") || false,
      title: button?.getAttribute("title"),
      count,
      countText: count?.textContent,
      disabled: button?.disabled || false,
    };
  }

  function restoreActionForm(state) {
    state.form.setAttribute("action", state.action);
    const button = state.form.querySelector("button");
    if (!button) return;
    setActive(button, state.active);
    if (state.title == null) button.removeAttribute("title");
    else button.setAttribute("title", state.title);
    if (state.count) state.count.textContent = state.countText;
    button.disabled = state.disabled;
  }

  function setActionFormActive(form, active) {
    const parts = statusActionParts(form);
    const button = form.querySelector("button");
    if (!parts || !button) return;
    setActive(button, active);
    const title = active ? button.dataset.activeTitle : button.dataset.inactiveTitle;
    if (title) button.setAttribute("title", title);
    form.setAttribute(
      "action",
      `/web/statuses/${parts.id}/${active ? "un" : ""}${form.dataset.action}`,
    );
  }

  function setActionPending(forms, pending, action) {
    forms.forEach((form) => {
      if (
        form.dataset.action !== action &&
        !(["upvote", "downvote"].includes(action) &&
          ["upvote", "downvote"].includes(form.dataset.action))
      ) return;
      const button = form.querySelector("button");
      if (button) button.disabled = pending;
    });
  }

  function updateVoteState(id, action, wasActive, scores) {
    const forms = statusActionForms(id);
    const up = forms.find((form) => form.dataset.action === "upvote");
    const down = forms.find((form) => form.dataset.action === "downvote");
    const oldUp = Boolean(up?.querySelector("button.is-active"));
    const oldDown = Boolean(down?.querySelector("button.is-active"));
    let nextUp = oldUp;
    let nextDown = oldDown;
    if (action === "upvote") {
      nextUp = !wasActive;
      if (nextUp) nextDown = false;
    } else {
      nextDown = !wasActive;
      if (nextDown) nextUp = false;
    }
    forms.forEach((other) => {
      if (other.dataset.action === "upvote") setActionFormActive(other, nextUp);
      if (other.dataset.action === "downvote") setActionFormActive(other, nextDown);
    });
    const delta = Number(nextUp) - Number(oldUp) - Number(nextDown) + Number(oldDown);
    scores.forEach((score) => {
      score.textContent = String((parseInt(score.textContent, 10) || 0) + delta);
    });
  }

  // ---- Poll voting without navigation ---------------------------------
  //
  // Poll choices are a real form and retain their PRG fallback. With JS, post
  // the vote once and then fetch the server-rendered poll region. Rendering
  // the results server-side keeps percentages, custom emoji, total votes and
  // the viewer's highlighted choices identical to a fresh page load.
  function bindPollForms(root) {
    root.querySelectorAll("[data-poll] form.poll").forEach((form) => {
      if (form.dataset.bound) return;
      form.dataset.bound = "1";
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        submitPollVote(form);
      });
    });
  }

  async function submitPollVote(form) {
    const region = form.closest("[data-poll]");
    const button = form.querySelector("button[type=submit]");
    if (!region || !button || region.dataset.pending) {
      return;
    }
    clearPollError(region);
    region.dataset.pending = "1";
    region.setAttribute("aria-busy", "true");
    button.disabled = true;
    const body = new URLSearchParams(new FormData(form));
    try {
      const res = await fetch(form.getAttribute("action"), {
        method: "POST",
        body,
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        redirect: "manual",
        credentials: "same-origin",
      });
      if (res.status !== 0 && !res.ok) {
        showPollError(region, region.dataset.pollFailed);
        return;
      }
      await refreshPoll(region.dataset.poll, body.get("return_to") || "");
    } catch (_err) {
      // The POST may have reached the server before the connection failed, so
      // never submit it a second time. Keep the choices and offer a retry.
      showPollError(region, region.dataset.pollNetworkFailed);
    } finally {
      delete region.dataset.pending;
      region.removeAttribute("aria-busy");
      button.disabled = false;
    }
  }

  async function refreshPoll(base, returnTo) {
    const regions = document.querySelectorAll(`[data-poll="${base}"]`);
    if (!regions.length) return;
    const res = await fetch(
      `${base}/poll?return_to=${encodeURIComponent(returnTo)}`,
      { headers: { Accept: "text/html" }, credentials: "same-origin" },
    );
    if (!res.ok) throw new Error(res.statusText);
    const tpl = document.createElement("template");
    tpl.innerHTML = await res.text();
    const fresh = tpl.content.querySelector("[data-poll]");
    if (!fresh) throw new Error("poll fragment missing");
    regions.forEach((old) => {
      const copy = fresh.cloneNode(true);
      old.replaceWith(copy);
      bindPollForms(copy);
    });
  }

  function clearPollError(region) {
    region.querySelectorAll(".poll__error").forEach((error) => error.remove());
  }

  function showPollError(region, message) {
    clearPollError(region);
    const error = document.createElement("p");
    error.className = "poll__error";
    error.setAttribute("role", "alert");
    error.textContent = message || "Your vote could not be recorded.";
    region.append(error);
  }

  // ---- Event RSVP without navigation ----------------------------------
  //
  // Attendance is not an optimistic two-state toggle: the same request may
  // become accepted immediately or wait for an organizer. Post in place, then
  // replace the RSVP cluster with the server's current state.
  function bindRsvpForms(root) {
    root.querySelectorAll("[data-rsvp] form.action-form").forEach((form) => {
      if (form.dataset.rsvpBound) return;
      form.dataset.rsvpBound = "1";
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        submitRsvp(form);
      });
    });
  }

  async function submitRsvp(form) {
    const region = form.closest("[data-rsvp]");
    const button = form.querySelector("button[type=submit]");
    if (!region || !button || region.dataset.pending) return;
    clearRsvpError(region);
    region.dataset.pending = "1";
    region.setAttribute("aria-busy", "true");
    button.disabled = true;
    const body = new URLSearchParams(new FormData(form));
    try {
      const res = await fetch(form.getAttribute("action"), {
        method: "POST",
        body,
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        redirect: "manual",
        credentials: "same-origin",
      });
      if (res.status !== 0 && !res.ok) {
        showRsvpError(region, region.dataset.rsvpFailed);
        return;
      }
      await refreshRsvp(region.dataset.rsvp, body.get("return_to") || "");
    } catch (_err) {
      showRsvpError(region, region.dataset.rsvpNetworkFailed);
    } finally {
      delete region.dataset.pending;
      region.removeAttribute("aria-busy");
      button.disabled = false;
    }
  }

  async function refreshRsvp(base, returnTo) {
    const regions = document.querySelectorAll(`[data-rsvp="${base}"]`);
    if (!regions.length) return;
    const res = await fetch(
      `${base}/rsvp?return_to=${encodeURIComponent(returnTo)}`,
      { headers: { Accept: "text/html" }, credentials: "same-origin" },
    );
    if (!res.ok) throw new Error(res.statusText);
    const tpl = document.createElement("template");
    tpl.innerHTML = await res.text();
    const fresh = tpl.content.querySelector("[data-rsvp]");
    if (!fresh) throw new Error("RSVP fragment missing");
    regions.forEach((old) => {
      const copy = fresh.cloneNode(true);
      old.replaceWith(copy);
      bindRsvpForms(copy);
    });
  }

  function clearRsvpError(region) {
    region.querySelectorAll(".status__event-rsvp-error").forEach((error) => error.remove());
  }

  function showRsvpError(region, message) {
    clearRsvpError(region);
    const error = document.createElement("span");
    error.className = "status__event-rsvp-error";
    error.setAttribute("role", "alert");
    error.textContent = message || "Your attendance response could not be saved.";
    region.append(error);
  }

  // ---- In-place status translation --------------------------------------
  //
  // The Translate control is a real <form>; without JS it 303s to the thread
  // permalink with ?translate=1 (server-rendered translated view). Here we
  // ask the same endpoint for JSON (X-Requested-With: fetch), stash the
  // original nodes, and swap the translation in place; the control becomes
  // "Show original", which restores the stash without another request.
  function bindTranslateForms(root) {
    root.querySelectorAll("form[data-translate]").forEach((form) => {
      if (form.dataset.bound) return;
      form.dataset.bound = "1";
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        translateStatus(form);
      });
    });
  }

  async function translateStatus(form) {
    const article = form.closest("article.status");
    // The container should be the .status__translation wrapper; fall back to
    // the parent so a markup surprise degrades to a less pretty inline error
    // rather than a page navigation.
    const note = form.closest(".status__translation") || form.parentElement;
    if (!article || !note) {
      form.submit();
      return;
    }
    // Already translated (or a request is running): nothing to do.
    if (form.hidden || form.dataset.pending) {
      return;
    }
    const button = form.querySelector("button");
    const label = button.textContent;
    form.dataset.pending = "1";
    button.disabled = true;
    button.classList.add("is-pending");
    button.textContent = form.dataset.i18nTranslating || "Translating…";
    clearTranslateError(note);
    try {
      const res = await fetch(form.action, {
        method: "POST",
        body: new URLSearchParams(new FormData(form)),
        headers: {
          "Content-Type": "application/x-www-form-urlencoded",
          "X-Requested-With": "fetch",
        },
        credentials: "same-origin",
      });
      const data = await res.json().catch(() => null);
      if (!res.ok || !data) {
        // Show the server's reason in place — navigating away to the
        // ?translate=1 page just repeats the failure with less context.
        showTranslateError(
          note,
          (data && data.error) ||
            form.dataset.i18nFailed ||
            "Translation failed.",
        );
        return;
      }
      applyTranslation(article, note, form, data);
    } catch (_err) {
      showTranslateError(
        note,
        form.dataset.i18nNetworkFailed ||
          "Translation failed — network error.",
      );
    } finally {
      delete form.dataset.pending;
      button.disabled = false;
      button.classList.remove("is-pending");
      button.textContent = label;
    }
  }

  function clearTranslateError(note) {
    note.querySelectorAll(".status__translation-error").forEach((el) => el.remove());
  }

  function showTranslateError(note, message) {
    clearTranslateError(note);
    const error = document.createElement("span");
    error.className = "status__translation-error";
    error.textContent = " " + message;
    note.append(error);
  }

  function applyTranslation(article, note, form, data) {
    const restores = [];
    const swapHtml = (el, html) => {
      if (!el || !html) return;
      const original = el.innerHTML;
      restores.push(() => { el.innerHTML = original; });
      el.innerHTML = html;
    };
    const swapText = (el, text) => {
      if (!el || !text) return;
      const original = el.innerHTML;
      restores.push(() => { el.innerHTML = original; });
      el.textContent = text;
    };
    // The main content div precedes any quoted status' in DOM order.
    swapHtml(article.querySelector(".status__content"), data.content);
    swapText(article.querySelector("details.status__cw > summary"), data.spoiler_text);
    const options = data.poll_options || [];
    if (options.length) {
      const labels = article.querySelectorAll(".poll__choice span, .poll__result .poll__title");
      labels.forEach((el, i) => swapText(el, options[i % options.length]));
    }

    form.hidden = true;
    // Defensive: never stack a second attribution if one is already shown.
    note.querySelectorAll(".status__translation-attribution").forEach((el) => el.remove());
    const attribution = document.createElement("span");
    attribution.className = "status__translation-attribution";
    attribution.textContent =
      (data.attribution || form.dataset.i18nTranslated || "Translated") + " · ";
    const revert = document.createElement("button");
    revert.type = "button";
    revert.className = "status__translation-link";
    revert.textContent = form.dataset.i18nShowOriginal || "Show original";
    revert.addEventListener("click", () => {
      restores.forEach((restore) => restore());
      attribution.remove();
      revert.remove();
      form.hidden = false;
    });
    note.append(attribution, revert);
  }

  function setActive(button, active) {
    button.classList.toggle("is-active", active);
    button.setAttribute("aria-pressed", String(active));
  }

  function bumpCount(button, delta) {
    const count = button.querySelector(".action__count");
    if (!count) return;
    const next = (parseInt(count.textContent, 10) || 0) + delta;
    count.textContent = String(Math.max(next, 0));
  }

  // ---- <details> disclosures --------------------------------------------
  //
  // The status "…" menu, the section selector (view::tab_strip) and the
  // mobile burger drawer are all <details> disclosures, so they open and
  // close without JS. This adds what a disclosure can't do on its own — only
  // one open at a time, close on outside click / Escape — plus, for the
  // popup-shaped ones, viewport-aware placement via the shared placePopup.
  //
  // `pop` is the selector of the floating panel to place (omit to skip
  // placement); `dismissed(menu, event)` decides whether a document click
  // should close the open disclosure. The default treats any click outside
  // the <details> as a dismissal. The drawer overrides it: its scrim fills
  // the viewport *inside* the <details>, so "outside" never fires — instead a
  // click whose target is the scrim itself (not the panel) closes it.
  function outsideOf(menu, event) {
    return !menu.contains(event.target);
  }

  function bindDisclosures(root, selector, { pop = null, dismissed = outsideOf } = {}) {
    root.querySelectorAll(selector).forEach((menu) => {
      if (menu.dataset.bound) return;
      menu.dataset.bound = "1";
      const summary = menu.querySelector("summary");
      const popEl = pop ? menu.querySelector(pop) : null;
      if (!summary || (pop && !popEl)) return;
      const opened = () => {
        if (!menu.open) return;
        // `DOMContentLoaded` can fire after a fast tap has already opened a
        // native <details>. Bind that existing open state exactly as if its
        // toggle event had happened after app.js initialized.
        if (menu.dataset.dismissBound) return;
        menu.dataset.dismissBound = "1";
        document.querySelectorAll(`${selector}[open]`).forEach((other) => {
          if (other !== menu) other.open = false;
        });
        if (popEl) placePopup(summary, popEl);
        const close = () => {
          menu.open = false;
        };
        const onDocClick = (event) => {
          if (dismissed(menu, event)) close();
        };
        const onKey = (event) => {
          if (event.key === "Escape") {
            close();
            summary.focus();
          }
        };
        const cleanup = () => {
          if (menu.open) return;
          delete menu.dataset.dismissBound;
          document.removeEventListener("click", onDocClick, true);
          document.removeEventListener("keydown", onKey);
          menu.removeEventListener("toggle", cleanup);
        };
        document.addEventListener("click", onDocClick, true);
        document.addEventListener("keydown", onKey);
        menu.addEventListener("toggle", cleanup);
      };
      menu.addEventListener("toggle", opened);
      if (menu.open) opened();
    });
  }

  function scrimTapped(menu, event) {
    return event.target === menu.querySelector("[data-drawer-scrim]");
  }

  // ---- Standalone-app external navigation -----------------------------
  // Signed-in content normally routes unknown links through `/web/go` so a
  // federated actor/post can resolve to its local copy. In an installed PWA,
  // that same-origin intermediate URL is inside the manifest scope: mobile
  // platforms launch a second Plamenu window, then turn it into an external
  // webview, whose Back/Close exits the app. Point those anchors at their
  // original off-site URL in standalone mode so the OS opens its ordinary
  // browser surface and leaves the existing Plamenu window beneath it.
  function bindStandaloneExternalLinks(root) {
    const standalone =
      window.matchMedia?.("(display-mode: standalone)").matches ||
      window.navigator.standalone === true;
    if (!standalone) return;
    root.querySelectorAll('a[href^="/web/go?"]').forEach((anchor) => {
      let external;
      try {
        external = new URL(anchor.href).searchParams.get("url");
        const parsed = new URL(external);
        if (!['http:', 'https:'].includes(parsed.protocol)) return;
      } catch (_err) {
        return;
      }
      anchor.href = external;
      anchor.target = "_blank";
      const rel = new Set((anchor.rel || "").split(/\s+/).filter(Boolean));
      rel.add("noopener");
      rel.add("noreferrer");
      anchor.rel = [...rel].join(" ");
    });
  }

  // "Copy link to post": inherently scripted, so the button ships hidden and
  // CSS reveals it under the `.js` root. Brief inline feedback, then the menu
  // closes itself.
  function bindCopyLinks(root) {
    root.querySelectorAll("[data-copy-link]").forEach((button) => {
      if (button.dataset.bound) return;
      button.dataset.bound = "1";
      button.addEventListener("click", async () => {
        try {
          await navigator.clipboard.writeText(button.dataset.copyLink);
        } catch (_err) {
          return; // clipboard denied: leave the menu open, nothing to undo
        }
        const label = button.textContent;
        button.textContent = "Copied!";
        setTimeout(() => {
          button.textContent = label;
          const menu = button.closest("details[data-status-menu]");
          if (menu) menu.open = false;
        }, 900);
      });
    });
  }

  // Destructive menu verbs (delete, redraft, block) carry data-confirm; gate
  // the POST behind a confirmation prompt. Without JS the form just submits.
  function bindConfirms(root) {
    root.querySelectorAll("form[data-confirm]").forEach((form) => {
      if (form.dataset.confirmBound) return;
      form.dataset.confirmBound = "1";
      form.addEventListener("submit", (event) => {
        // Read at submit time: the moderation toggle below removes the
        // prompt while a form is flipped to its (harmless) undo direction.
        if (form.dataset.confirm && !window.confirm(form.dataset.confirm)) {
          event.preventDefault();
        }
      });
    });
  }

  // ---- In-place moderation (mute / block / domain block) ----------------
  //
  // The status-menu moderation verbs are real POST forms marked data-mod.
  // Following their 303 would land the reader back on page one of the feed,
  // losing their place, so we fire the POST in the background instead, dim
  // every visible card by the sanctioned account (or server) as feedback,
  // and flip the form to the undo verb — an accidental mute stays one click
  // to take back. Dimmed cards remain fully interactable. bindConfirms is
  // bound first on the same form; a cancelled confirm arrives here with
  // defaultPrevented already set.
  function bindModerationForms(root) {
    root.querySelectorAll("form[data-mod]").forEach((form) => {
      if (form.dataset.modBound) return;
      form.dataset.modBound = "1";
      form.addEventListener("submit", (event) => {
        if (event.defaultPrevented) return;
        event.preventDefault();
        submitModeration(form);
      });
    });
  }

  async function submitModeration(form) {
    try {
      const body = new URLSearchParams(new FormData(form));
      const res = await fetch(form.getAttribute("action"), {
        method: "POST",
        body,
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        redirect: "manual",
      });
      if (res.status !== 0 && !res.ok) throw new Error(res.statusText);
    } catch (_err) {
      form.submit(); // fall back to the no-JS path: full POST + redirect
      return;
    }
    setModerationState(form, form.dataset.modActive !== "1");
    const menu = form.closest("details[data-status-menu]");
    if (menu) menu.open = false;
  }

  // Applies (or undoes) a moderation verb's client-side effect: dims every
  // article the target authored or boosted, and flips every form for the
  // same verb+target — the account can appear in several cards — so they
  // all stay in sync.
  function setModerationState(form, active) {
    const target = form.dataset.modAccount || form.dataset.modDomain;
    const attrs = form.dataset.modAccount
      ? ["data-author-id", "data-booster-id"]
      : ["data-author-domain", "data-booster-domain"];
    const selector = attrs
      .map((attr) => `article.status[${attr}="${target}"]`)
      .join(", ");
    document.querySelectorAll(selector).forEach((article) => {
      article.classList.toggle("is-moderated", active);
    });
    // A card merging several boosts of one post carries all of them in a
    // space-separated data-booster-* attribute, so the exact-match selector
    // above deliberately misses it: the post is in the feed on the other
    // boosters' account too. Dim the muted name instead.
    const names = attrs
      .map((attr) => `.status__boosters a[${attr}="${target}"]`)
      .join(", ");
    document.querySelectorAll(names).forEach((name) => {
      name.classList.toggle("is-moderated", active);
    });
    document
      .querySelectorAll(`form[data-mod="${form.dataset.mod}"]`)
      .forEach((other) => {
        if ((other.dataset.modAccount || other.dataset.modDomain) !== target) return;
        const button = other.querySelector("button[type=submit]");
        if (!other.dataset.modDoAction) {
          other.dataset.modDoAction = other.getAttribute("action");
          other.dataset.modDoLabel = button.textContent;
          if (other.dataset.confirm) other.dataset.modDoConfirm = other.dataset.confirm;
        }
        other.setAttribute(
          "action",
          active ? other.dataset.modUndoAction : other.dataset.modDoAction,
        );
        button.textContent = active
          ? other.dataset.modUndoLabel
          : other.dataset.modDoLabel;
        // The undo direction is harmless: no confirmation, no danger tint.
        if (other.dataset.modDoConfirm) {
          if (active) delete other.dataset.confirm;
          else other.dataset.confirm = other.dataset.modDoConfirm;
          button.classList.toggle("is-danger", !active);
        }
        other.dataset.modActive = active ? "1" : "";
      });
  }

  // The report form: citing a rule files the report under "it
  // violates server rules" regardless of the chosen category, so checking one
  // selects that radio to keep the form honest about what gets submitted.
  function bindReportForms(root) {
    root.querySelectorAll("form.report-form").forEach((form) => {
      if (form.dataset.reportBound) return;
      form.dataset.reportBound = "1";
      const violation = form.querySelector(
        'input[name="category"][value="violation"]',
      );
      if (!violation) return;
      form.querySelectorAll('input[name="rule_ids[]"]').forEach((box) => {
        box.addEventListener("change", () => {
          if (box.checked) violation.checked = true;
        });
      });
    });
  }

  // Marking an announcement read is a local acknowledgement, so navigating
  // the whole timeline/listing is needless. On the home banner the card goes
  // away; on the archive page it stays visible and simply loses unread state.
  function bindAnnouncementDismiss(root) {
    root.querySelectorAll("form[data-announcement-dismiss]").forEach((form) => {
      if (form.dataset.dismissBound) return;
      form.dataset.dismissBound = "1";
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        dismissAnnouncement(form);
      });
    });
  }

  async function dismissAnnouncement(form) {
    const article = form.closest("article[data-announcement]");
    const button = form.querySelector("button[type=submit]");
    if (!article || !button || form.dataset.pending) return;
    form.dataset.pending = "1";
    button.disabled = true;
    article.querySelectorAll(".announcement__error").forEach((el) => el.remove());
    try {
      const res = await fetch(form.getAttribute("action"), {
        method: "POST",
        body: new URLSearchParams(new FormData(form)),
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        redirect: "manual",
        credentials: "same-origin",
      });
      if (res.status !== 0 && !res.ok) throw new Error(res.statusText);
    } catch (_err) {
      const error = document.createElement("span");
      error.className = "announcement__error";
      error.setAttribute("role", "alert");
      error.textContent = form.dataset.dismissFailed || "Could not mark as read.";
      form.append(error);
      delete form.dataset.pending;
      button.disabled = false;
      return;
    }

    const banner = article.closest("[data-announcements-banner]");
    if (banner) {
      article.remove();
      if (!banner.querySelector("article[data-announcement]")) {
        const note = document.createElement("p");
        note.className = "announcements-link";
        const link = document.createElement("a");
        link.href = "/announcements";
        link.textContent = banner.dataset.emptyLabel || "Announcements";
        note.append(link);
        banner.replaceWith(note);
      }
      return;
    }
    article.classList.remove("is-unread");
    article.querySelector(".announcement__unread")?.remove();
    form.remove();
  }

  // ---- Live relative timestamps ----------------------------------------
  function relative(iso) {
    const then = Date.parse(iso);
    if (Number.isNaN(then)) return null;
    const s = Math.max(0, Math.round((Date.now() - then) / 1000));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.floor(s / 60)}m`;
    if (s < 86400) return `${Math.floor(s / 3600)}h`;
    if (s < 2592000) return `${Math.floor(s / 86400)}d`;
    return `${Math.floor(s / 2592000)}mo`;
  }

  function refreshTimes(root) {
    // `data-absolute` opts out: the edit-history page keeps full timestamps.
    // The tooltip is never synthesised here: the server renders every `<time>`
    // with a complete `title` carrying both the viewer-local and the UTC
    // reading, and guessing one from the body would replace it with whatever
    // coarse label happened to be showing.
    root.querySelectorAll("time[datetime]:not([data-absolute])").forEach((el) => {
      const label = relative(el.getAttribute("datetime"));
      if (label) el.textContent = label;
    });
  }

  // ---- Compose: auto-grow, character count, section toggles ------------
  // The instance character limit rides on the textarea (data-max-chars).
  const DEFAULT_MAX_CHARS = 5000;

  // Weighted length, matching the server's `compose::countable_length`
  // (Mastodon's StatusLengthValidator): every URL counts as a fixed 23, a
  // remote mention's @domain is free, spoiler text is included, and the result
  // is measured in grapheme clusters — so the counter agrees with what the
  // backend will accept rather than raw `.length`.
  const URL_PLACEHOLDER = "xxxxxxxxxxxxxxxxxxxxxxx"; // 23 chars
  const URL_RE = /https?:\/\/\S+/gi;
  const MENTION_RE = /(^|[^/\w])@(([a-z0-9_]+)@[a-z0-9.-]+[a-z0-9]+)/gi;
  const segmenter =
    typeof Intl !== "undefined" && Intl.Segmenter ? new Intl.Segmenter() : null;

  function graphemeCount(str) {
    if (!str) return 0;
    if (!segmenter) return [...str].length; // code points: close enough
    let n = 0;
    for (const _ of segmenter.segment(str)) n++;
    return n;
  }

  function weightedLength(spoiler, text) {
    const countable = text
      .replace(URL_RE, URL_PLACEHOLDER)
      .replace(MENTION_RE, "$1@$3");
    return graphemeCount(spoiler + countable);
  }

  // Collapse a composer's [data-compose-section] groups behind toolbar toggle
  // buttons. Without JS the sections stay visible (CSS hides them only under
  // the `.js` root); here a section opens if it already holds a value (so a
  // pre-filled CW or re-rendered draft isn't hidden) and otherwise starts shut.
  function setSectionOpen(button, section, open) {
    section.classList.toggle("is-open", open);
    button.classList.toggle("is-active", open);
    button.setAttribute("aria-expanded", String(open));
  }

  function bindComposeToggles(form) {
    form.querySelectorAll("[data-compose-toggle]").forEach((button) => {
      const name = button.dataset.composeToggle;
      const section = form.querySelector(`[data-compose-section="${name}"]`);
      if (!section) return;
      const filled = [...section.querySelectorAll("input, textarea")].some(
        (el) =>
          (el.type === "text" || el.type === "datetime-local") && el.value.trim()
      );
      setSectionOpen(button, section, filled);
      button.addEventListener("click", () => {
        const open = !section.classList.contains("is-open");
        setSectionOpen(button, section, open);
        if (open) {
          const field = section.querySelector("input, textarea, select");
          if (field) field.focus();
        }
      });
    });
  }

  // ---- Compose: custom selector menus ---------------------------------
  //
  // Visibility, quote-policy and language render a native <select> (kept as the
  // submitted value and the no-JS fallback) plus a custom trigger + popup that
  // can show per-option icons/descriptions a native <select> can't. Selecting an
  // option writes the value back into the <select> and closes the popup. Only
  // one popup is open at a time.
  let closeOpenPopup = null;

  // Flip the popup above/below the trigger by whichever side has room (the
  // composer toolbar can sit near the top of the viewport in the /compose
  // view, where opening upward would clip), and right-align it when a
  // left-aligned popup would spill past the viewport's right edge. On phones
  // the fixed tab bar is not usable popup space: a taller moderation menu
  // should flip upward rather than tuck its final row underneath the bar.
  function placePopup(trigger, pop) {
    pop.classList.remove("is-above", "is-below", "is-end");
    const rect = trigger.getBoundingClientRect();
    const height = pop.offsetHeight;
    const tabbar = document.querySelector(".tabbar");
    const usableBottom =
      tabbar && getComputedStyle(tabbar).display !== "none"
        ? Math.min(window.innerHeight, tabbar.getBoundingClientRect().top)
        : window.innerHeight;
    const spaceBelow = usableBottom - rect.bottom;
    const spaceAbove = rect.top;
    const below = spaceBelow >= height + 8 || spaceBelow >= spaceAbove;
    pop.classList.add(below ? "is-below" : "is-above");
    if (rect.left + pop.offsetWidth > window.innerWidth - 8) {
      pop.classList.add("is-end");
    }
  }

  function openPopup(trigger, pop, onOpen) {
    if (closeOpenPopup) closeOpenPopup();
    pop.hidden = false;
    trigger.setAttribute("aria-expanded", "true");
    function close() {
      pop.hidden = true;
      trigger.setAttribute("aria-expanded", "false");
      pop.classList.remove("is-above", "is-below", "is-end");
      document.removeEventListener("click", onDocClick, true);
      document.removeEventListener("keydown", onKey);
      window.removeEventListener("resize", onReflow);
      closeOpenPopup = null;
    }
    function onDocClick(event) {
      if (!pop.contains(event.target) && !trigger.contains(event.target)) close();
    }
    function onKey(event) {
      if (event.key === "Escape") {
        close();
        trigger.focus();
      }
    }
    function onReflow() {
      placePopup(trigger, pop);
    }
    // Populate before measuring so the flip sees the popup's real height.
    if (onOpen) onOpen();
    placePopup(trigger, pop);
    // Defer the doc listener so the click that opened the popup can't close it.
    setTimeout(() => document.addEventListener("click", onDocClick, true), 0);
    document.addEventListener("keydown", onKey);
    window.addEventListener("resize", onReflow);
    closeOpenPopup = close;
  }

  // A trigger that toggles its own popup: closes if already open, else opens.
  function togglePopup(trigger, pop, onOpen) {
    trigger.addEventListener("click", () => {
      if (trigger.getAttribute("aria-expanded") === "true") {
        if (closeOpenPopup) closeOpenPopup();
        return;
      }
      openPopup(trigger, pop, onOpen);
    });
  }

  function bindComposeMenus(form) {
    form.querySelectorAll("[data-compose-menu]").forEach((menu) => {
      const select = menu.querySelector("select");
      const trigger = menu.querySelector("[data-menu-trigger]");
      const pop = menu.querySelector("[data-menu-pop]");
      const iconSlot = menu.querySelector("[data-menu-icon]");
      if (!select || !trigger || !pop) return;
      const options = [...pop.querySelectorAll("[data-value]")];

      togglePopup(trigger, pop, () => {
        const active = options.find((o) => o.dataset.value === select.value);
        if (active) active.focus();
      });

      options.forEach((option) => {
        option.addEventListener("click", () => {
          select.value = option.dataset.value;
          select.dispatchEvent(new Event("change", { bubbles: true }));
          options.forEach((o) =>
            o.setAttribute("aria-selected", String(o === option))
          );
          const optIcon = option.querySelector(".compose__menu-option-icon");
          if (iconSlot && optIcon) iconSlot.innerHTML = optIcon.innerHTML;
          if (closeOpenPopup) closeOpenPopup();
          trigger.focus();
        });
      });
    });
  }

  // Also bound outside composer forms: the settings pages reuse the combo
  // for their language fields, so `enhance` calls this on the whole page.
  function bindComposeCombo(form) {
    form.querySelectorAll("[data-compose-combo]").forEach((combo) => {
      if (combo.dataset.bound) return;
      combo.dataset.bound = "1";
      const select = combo.querySelector("select");
      const trigger = combo.querySelector("[data-combo-trigger]");
      const pop = combo.querySelector("[data-combo-pop]");
      const search = combo.querySelector("[data-combo-search]");
      const list = combo.querySelector("[data-combo-list]");
      const labelSlot = combo.querySelector("[data-combo-label]");
      if (!select || !trigger || !pop || !list) return;

      const entries = [...select.options].map((option) => ({
        value: option.value,
        label: option.textContent,
      }));

      const render = (filter) => {
        const needle = filter.trim().toLowerCase();
        const kept = needle
          ? entries.filter(
              (entry) =>
                entry.label.toLowerCase().includes(needle) ||
                entry.value.toLowerCase().includes(needle)
            )
          : entries;
        list.replaceChildren();
        if (!kept.length) {
          const empty = document.createElement("li");
          empty.className = "compose__combo-empty";
          empty.textContent = combo.dataset.emptyLabel || "No languages found";
          list.append(empty);
          return;
        }
        kept.forEach((entry) => {
          const li = document.createElement("li");
          const button = document.createElement("button");
          button.type = "button";
          button.className = "compose__combo-option";
          button.setAttribute("role", "option");
          button.dataset.value = entry.value;
          button.textContent = entry.label;
          button.setAttribute("aria-selected", String(entry.value === select.value));
          button.addEventListener("click", () => {
            select.value = entry.value;
            select.dispatchEvent(new Event("change", { bubbles: true }));
            if (labelSlot) labelSlot.textContent = entry.label;
            if (closeOpenPopup) closeOpenPopup();
            trigger.focus();
          });
          li.append(button);
          list.append(li);
        });
      };

      togglePopup(trigger, pop, () => {
        render(search ? search.value : "");
        if (search) search.focus();
      });

      if (search) search.addEventListener("input", () => render(search.value));
    });
  }

  // ---- Compose: custom emoji picker -----------------------------------
  //
  // A toolbar button opens a searchable grid of the instance's custom emoji
  // (from the authenticated built-in catalog, fetched once per page and shared across
  // composers), grouped by their admin-assigned category. Picking one inserts
  // `:shortcode:` at the caret of the post body — or the CW field, if that was
  // written in last. Inserting at a caret is inherently JS, so there is no
  // no-JS variant; the trigger stays hidden without the `.js` root.
  let emojiCatalog = null; // Promise for the /web/custom-emojis payload

  function loadEmojiCatalog() {
    emojiCatalog ||= fetch("/web/custom-emojis", {
      headers: { Accept: "application/json" },
    })
      .then((res) => {
        if (!res.ok) throw new Error(res.statusText);
        return res.json();
      })
      .catch((err) => {
        emojiCatalog = null; // let the next open retry
        throw err;
      });
    return emojiCatalog;
  }

  // Groups custom emoji by their admin-assigned category, as [name, entries]
  // pairs: named categories A→Z first, the uncategorized bucket last (the
  // Sharkey/pleroma-fe picker convention). Entries arrive shortcode-sorted
  // from the API, so each group stays alphabetical.
  function emojiGroups(entries) {
    const groups = new Map();
    for (const emoji of entries) {
      const key = emoji._personal ? "\u0000personal" : emoji.category || "";
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(emoji);
    }
    const names = [...groups.keys()].sort((a, b) =>
      a === "\u0000personal" ? -1 : b === "\u0000personal" ? 1 :
      a === "" ? 1 : b === "" ? -1 : a.localeCompare(b)
    );
    return names.map((name) => [name === "\u0000personal" ? "Your emoji" : name, groups.get(name)]);
  }

  function insertAtCaret(field, text) {
    const start = field.selectionStart ?? field.value.length;
    const end = field.selectionEnd ?? start;
    const before = field.value.slice(0, start);
    // Pad like Mastodon: a space on each side unless one is already there.
    const lead = before && !/\s$/.test(before) ? " " : "";
    const inserted = `${lead}${text} `;
    field.value = before + inserted + field.value.slice(end);
    field.focus();
    const caret = start + inserted.length;
    field.setSelectionRange(caret, caret);
    // Notify the counter/autogrow listeners.
    field.dispatchEvent(new Event("input", { bubbles: true }));
  }

  function bindComposeEmoji(form) {
    const picker = form.querySelector("[data-compose-emoji]");
    if (!picker) return;
    const trigger = picker.querySelector("[data-emoji-trigger]");
    const pop = picker.querySelector("[data-emoji-pop]");
    const search = picker.querySelector("[data-emoji-search]");
    const list = picker.querySelector("[data-emoji-list]");
    const textarea = form.querySelector("textarea[name=status]");
    if (!trigger || !pop || !search || !list || !textarea) return;

    // Emoji also render in the content warning, so the shortcode lands in
    // whichever of the two fields was focused last (the body by default).
    let target = textarea;
    const spoiler = form.querySelector("[data-compose-spoiler]");
    form.addEventListener("focusin", (event) => {
      if (event.target === textarea || event.target === spoiler) {
        target = event.target;
      }
    });

    let catalog = null;

    function note(text) {
      const p = document.createElement("p");
      p.className = "compose__emoji-note";
      p.textContent = text;
      list.replaceChildren(p);
    }

    function option(emoji) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "compose__emoji-option";
      button.title = `:${emoji.shortcode}:`;
      button.setAttribute("aria-label", `Insert :${emoji.shortcode}:`);
      const img = document.createElement("img");
      img.src = emoji.url;
      img.alt = "";
      img.loading = "lazy";
      button.append(img);
      button.addEventListener("click", () => {
        insertAtCaret(target, `:${emoji.shortcode}:`);
        if (closeOpenPopup) closeOpenPopup();
      });
      return button;
    }

    function render() {
      if (!catalog.length) {
        note("This server has no custom emoji yet.");
        return;
      }
      const needle = search.value.trim().toLowerCase().replaceAll(":", "");
      const kept = needle
        ? catalog.filter((e) => e.shortcode.toLowerCase().includes(needle))
        : catalog;
      if (!kept.length) {
        note("No matching emoji.");
        return;
      }
      const grouped = emojiGroups(kept);
      list.replaceChildren();
      for (const [name, entries] of grouped) {
        // A heading per group — unless everything is uncategorized, where a
        // lone "Custom" label would be noise.
        if (grouped.length > 1 || name) {
          const heading = document.createElement("div");
          heading.className = "compose__emoji-heading";
          heading.textContent = name || "Custom";
          list.append(heading);
        }
        const grid = document.createElement("div");
        grid.className = "compose__emoji-grid";
        grid.append(...entries.map(option));
        list.append(grid);
      }
    }

    togglePopup(trigger, pop, () => {
      if (catalog) {
        render();
        search.focus();
        return;
      }
      note("Loading…");
      loadEmojiCatalog().then(
        (loaded) => {
          catalog = loaded;
          if (pop.hidden) return; // closed before the fetch came back
          render();
          placePopup(trigger, pop); // re-measure at the grid's real height
          search.focus();
        },
        () => {
          if (!pop.hidden) note("Couldn't load emoji — close and retry.");
        }
      );
    });

    search.addEventListener("input", () => {
      if (catalog) render();
    });
  }

  // ---- Emoji reaction chips --------------------------------------------
  //
  // Reaction chips and the picker link work without JS. Here we enhance that
  // link into an on-card popup. The complete Unicode catalog is a separate,
  // immutable asset rather than 186 KB embedded in app.js; it and the dynamic
  // custom-emoji listing are fetched only on the first picker open.
  let unicodeReactionCatalog = null;

  function loadUnicodeReactionCatalog(url) {
    unicodeReactionCatalog ||= fetch(url, {
      headers: { Accept: "application/json" },
    })
      .then((res) => {
        if (!res.ok) throw new Error(res.statusText);
        return res.json();
      })
      .catch((err) => {
        unicodeReactionCatalog = null;
        throw err;
      });
    return unicodeReactionCatalog;
  }

  function bindReactionPickers(root) {
    root.querySelectorAll("[data-reaction-picker]").forEach((picker) => {
      if (picker.dataset.bound) return;
      picker.dataset.bound = "1";
      const trigger = picker.querySelector("[data-emoji-trigger]");
      const pop = picker.querySelector("[data-emoji-pop]");
      const search = picker.querySelector("[data-emoji-search]");
      const list = picker.querySelector("[data-emoji-list]");
      const form = picker.querySelector("[data-react-form]");
      // The web verb base path (`/web/statuses/{id}` or
      // `/web/announcements/{id}`) — the same key the chip row carries.
      const base = picker.dataset.reactBase;
      const unicodeUrl = picker.dataset.unicodeCatalog;
      if (!trigger || !pop || !search || !list || !form || !base || !unicodeUrl) return;

      let groups = null;

      function react(name) {
        const action = `${base}/react/${encodeURIComponent(name)}`;
        if (closeOpenPopup) closeOpenPopup();
        submitReaction(action, form).catch(() => {
          // Fall back to the no-JS path: a plain submit that reloads via PRG.
          form.action = action;
          form.submit();
        });
      }

      function note(text, fallback) {
        const p = document.createElement("p");
        p.className = "compose__emoji-note";
        p.textContent = text;
        list.append(p);
        if (fallback) {
          const link = document.createElement("a");
          link.className = "compose__emoji-fallback";
          link.href = trigger.href;
          link.textContent = "Open the full picker";
          list.append(link);
        }
      }

      function option(entry) {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "compose__emoji-option";
        button.title = entry.imageUrl
          ? `:${entry.value}:`
          : `${entry.value} — ${entry.name}`;
        button.setAttribute("aria-label", `React with ${button.title}`);
        if (entry.imageUrl) {
          const img = document.createElement("img");
          img.src = entry.imageUrl;
          img.alt = "";
          img.loading = "lazy";
          button.append(img);
        } else {
          button.append(entry.value);
        }
        button.addEventListener("click", () => react(entry.value));
        return button;
      }

      function grid(entries) {
        const el = document.createElement("div");
        el.className = "compose__emoji-grid";
        el.append(...entries);
        return el;
      }

      function heading(text) {
        const el = document.createElement("div");
        el.className = "compose__emoji-heading";
        el.textContent = text;
        return el;
      }

      function renderGroup(group, needle, includeGroupHeading) {
        let rendered = 0;
        const fragment = document.createDocumentFragment();
        for (const section of group.sections) {
          const entries = needle
            ? section.entries.filter((entry) =>
                `${entry.value} ${entry.name} ${section.name} ${group.name}`
                  .toLowerCase()
                  .includes(needle)
              )
            : section.entries;
          if (!entries.length) continue;
          if (includeGroupHeading && rendered === 0) {
            fragment.append(heading(group.name));
          }
          if (section.name) fragment.append(heading(section.name));
          fragment.append(grid(entries.map(option)));
          rendered += entries.length;
        }
        if (rendered) list.append(fragment);
        return rendered;
      }

      function render() {
        const needle = search.value.trim().toLowerCase().replaceAll(":", "");
        list.replaceChildren();
        if (!groups) return;
        let matches = 0;
        for (const group of groups) {
          matches += renderGroup(group, needle, true);
        }
        if (!matches) note("No matching emoji.");
      }

      function combinedGroups(unicode, custom) {
        const combined = emojiGroups(custom).map(([name, entries]) => ({
          name: name ? `Custom · ${name}` : "Custom",
          sections: [{
            name: "",
            entries: entries.map((entry) => ({
              value: entry.shortcode,
              name: entry.shortcode,
              imageUrl: entry.url,
            })),
          }],
        }));
        for (const group of unicode.groups || []) {
          combined.push({
            name: group.name,
            sections: (group.subgroups || []).map((section) => ({
              name: section.name.replaceAll("-", " "),
              entries: section.emoji.map(([value, name]) => ({ value, name })),
            })),
          });
        }
        return combined;
      }

      // Preserve the anchor's navigation as the baseline, intercepting only
      // after this picker has successfully bound as an enhancement.
      trigger.addEventListener("click", (event) => event.preventDefault());
      togglePopup(trigger, pop, () => {
        if (groups) {
          render();
          search.focus();
          return;
        }
        list.replaceChildren();
        note("Loading…");
        Promise.all([
          loadUnicodeReactionCatalog(unicodeUrl),
          loadEmojiCatalog().catch(() => []),
        ]).then(
          ([unicode, custom]) => {
            groups = combinedGroups(unicode, custom);
            if (pop.hidden) return; // closed before the fetch came back
            render();
            placePopup(trigger, pop); // re-measure at the grid's real height
            search.focus();
          },
          () => {
            if (pop.hidden) return;
            list.replaceChildren();
            note("Couldn't load emoji.", true);
          }
        );
      });

      search.addEventListener("input", () => {
        if (groups) render();
      });
    });
  }

  // ---- Reactions without a reload (fetch + row swap) -------------------
  //
  // A reaction changes more than a count: chips appear, disappear and flip
  // between their react/unreact forms. Rather than mirroring that rendering
  // here, the fetch path POSTs like the plain form would, then re-fetches
  // the server-rendered chip row and swaps it in place.
  function bindReactionForms(root) {
    root.querySelectorAll("form.reaction-form").forEach((form) => {
      if (form.dataset.bound) return;
      form.dataset.bound = "1";
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        submitReaction(form.getAttribute("action"), form).catch(() => form.submit());
      });
    });
  }

  async function submitReaction(action, form) {
    // Both reactable kinds share the path shape `{base}/(un)react/{name}`,
    // with `base` also the chip row's key and the fragment endpoint's root.
    const base = action.match(/^(.+)\/(?:un)?react\//)?.[1];
    if (!base) throw new Error("unrecognised action path");
    const body = new URLSearchParams(new FormData(form));
    const res = await fetch(action, {
      method: "POST",
      body,
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      redirect: "manual",
    });
    // `redirect: manual` reports the 303 back as an opaque status 0.
    if (res.status !== 0 && !res.ok) throw new Error(res.statusText);
    await refreshReactions(base, body.get("return_to") || "");
  }

  async function refreshReactions(base, returnTo) {
    const rows = document.querySelectorAll(`[data-reactions="${base}"]`);
    if (!rows.length) return;
    const res = await fetch(
      `${base}/reactions?return_to=${encodeURIComponent(returnTo)}`,
      { headers: { Accept: "text/html" } }
    );
    if (!res.ok) return;
    const tpl = document.createElement("template");
    tpl.innerHTML = await res.text();
    const fresh = tpl.content.querySelector("[data-reactions]");
    if (!fresh) return;
    // The same status can be on the page more than once (a post and its
    // boost); every copy of the row gets the swap.
    rows.forEach((row) => {
      const copy = fresh.cloneNode(true);
      row.replaceWith(copy);
      bindReactionForms(copy);
      bindReactionTooltips(copy.parentElement || document);
    });
  }

  // Reactor names on expand: hovering or focusing a chip fetches the Pleroma
  // reactors listing once and folds the handles into the chip's tooltip.
  // Status rows only — announcements have no reactors API.
  function bindReactionTooltips(root) {
    root.querySelectorAll("[data-reactions]").forEach((row) => {
      if (row.dataset.tipsBound) return;
      row.dataset.tipsBound = "1";
      const statusId = row.dataset.reactions.match(/^\/web\/statuses\/(\d+)$/)?.[1];
      if (!statusId) return;
      row.querySelectorAll("[data-reaction-name]").forEach((chip) => {
        const name = chip.dataset.reactionName;
        let loaded = false;
        const load = async () => {
          if (loaded) return;
          loaded = true;
          try {
            const res = await fetch(
              `/api/v1/pleroma/statuses/${statusId}/reactions/${encodeURIComponent(name)}`,
              { headers: { Accept: "application/json" } }
            );
            if (!res.ok) return;
            const groups = await res.json();
            const accounts = groups[0]?.accounts || [];
            if (!accounts.length) return;
            const shown = accounts.slice(0, 10).map((a) => `@${a.acct}`);
            const more = accounts.length - shown.length;
            chip.title =
              shown.join(", ") + (more > 0 ? ` and ${more} more` : "");
          } catch (_err) {
            loaded = false; // transient failure: retry on the next hover
          }
        };
        chip.addEventListener("mouseenter", load);
        chip.addEventListener("focusin", load);
      });
    });
  }

  // ---- Compose: dynamic media attachments -----------------------------
  //
  // Without JS the media section is a fixed stack of alt+file slots. Here we
  // remove those and drive a dynamic list: files arrive by clicking the
  // dropzone, dragging onto the composer, or pasting, and each becomes a card
  // with a thumbnail, an editable alt-text box and a remove button. The submit
  // path is unchanged — each card carries a hidden `media[]` file input (its
  // File set via DataTransfer) preceded in document order by its `media_alt[]`
  // box, so the backend's order-based alt/file pairing still holds.
  const MEDIA_ACCEPT = "image/*,video/*,audio/*";

  // Feather-style glyphs matching the server sprite, for the JS-built controls.
  const ICON_REMOVE = '<path d="M18 6 6 18"/><path d="M6 6l12 12"/>';
  const ICON_ADD = '<path d="M12 5v14"/><path d="M5 12h14"/>';
  const ICON_FILM =
    '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 4v16"/>' +
    '<path d="M17 4v16"/><path d="M3 9h4"/><path d="M17 9h4"/><path d="M3 15h4"/>' +
    '<path d="M17 15h4"/>';
  const ICON_AUDIO =
    '<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/>' +
    '<circle cx="18" cy="16" r="3"/>';

  function svgIcon(paths) {
    return (
      '<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" ' +
      'stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" ' +
      'aria-hidden="true">' +
      paths +
      "</svg>"
    );
  }

  function openSection(form, name) {
    const button = form.querySelector(`[data-compose-toggle="${name}"]`);
    const section = form.querySelector(`[data-compose-section="${name}"]`);
    if (button && section) setSectionOpen(button, section, true);
  }

  function bindComposeMedia(form) {
    const section = form.querySelector('[data-compose-section="media"]');
    if (!section) return;
    const body = section.querySelector(".compose__media-body");
    const staticSlots = section.querySelector("[data-media-static]");
    if (!body || !staticSlots) return;
    const max = parseInt(section.dataset.mediaMax, 10) || 4;

    // Drop the no-JS slots; we submit our own inputs instead.
    staticSlots.remove();

    const items = []; // { file, li, url }

    const list = document.createElement("ul");
    list.className = "compose__media-list";
    list.setAttribute("role", "list");

    const picker = document.createElement("input");
    picker.type = "file";
    picker.accept = MEDIA_ACCEPT;
    picker.multiple = true;
    picker.className = "visually-hidden";
    // No `name`, so the picker itself never submits — the per-card inputs do.

    const dropzone = document.createElement("button");
    dropzone.type = "button";
    dropzone.className = "compose__dropzone";
    dropzone.innerHTML = svgIcon(
        '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/>' +
          '<path d="M17 8l-5-5-5 5"/><path d="M12 3v12"/>'
      );
    const dropzoneLabel = document.createElement("span");
    dropzoneLabel.append(
      document.createTextNode(
        form.dataset.i18nMediaAdd || "Add images, video or audio"
      ),
      document.createElement("br")
    );
    const dropzoneHelp = document.createElement("small");
    dropzoneHelp.textContent =
      form.dataset.i18nMediaDropHelp || "Click, drag, or paste";
    dropzoneLabel.append(dropzoneHelp);
    dropzone.append(dropzoneLabel);

    body.prepend(list, dropzone, picker);

    function refreshAddState() {
      const full = items.length >= max;
      dropzone.hidden = full;
      picker.disabled = full;
    }

    function removeItem(item) {
      const idx = items.indexOf(item);
      if (idx === -1) return;
      items.splice(idx, 1);
      if (item.url) URL.revokeObjectURL(item.url);
      item.li.remove();
      refreshAddState();
    }

    function makeCard(file) {
      const li = document.createElement("li");
      li.className = "compose__attachment";

      const preview = document.createElement("div");
      preview.className = "compose__thumb";
      let url = null;
      if (file.type.startsWith("image/")) {
        url = URL.createObjectURL(file);
        const img = document.createElement("img");
        img.src = url;
        img.alt = "";
        preview.appendChild(img);
      } else {
        preview.innerHTML = svgIcon(
          file.type.startsWith("video/") ? ICON_FILM : ICON_AUDIO
        );
      }

      const alt = document.createElement("textarea");
      alt.name = "media_alt[]";
      alt.rows = 2;
      alt.maxLength = 1500;
      alt.className = "compose__alt";
      alt.placeholder =
        form.dataset.i18nAltDescription ||
        "Describe for people who are blind or have low vision";
      alt.setAttribute(
        "aria-label",
        (form.dataset.i18nAltNamed || "Alt text for __name__").replace(
          "__name__",
          file.name
        )
      );

      // The File rides on a real file input so the plain form submit carries it;
      // it follows its alt box in document order, matching the backend pairing.
      const fileInput = document.createElement("input");
      fileInput.type = "file";
      fileInput.name = "media[]";
      fileInput.className = "visually-hidden";
      const dt = new DataTransfer();
      dt.items.add(file);
      fileInput.files = dt.files;

      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "compose__attachment-remove";
      remove.innerHTML = svgIcon(ICON_REMOVE);
      remove.setAttribute(
        "aria-label",
        (form.dataset.i18nRemoveNamed || "Remove __name__").replace(
          "__name__",
          file.name
        )
      );

      const bodyEl = document.createElement("div");
      bodyEl.className = "compose__attachment-body";
      bodyEl.append(alt, fileInput);

      li.append(preview, bodyEl, remove);

      const item = { file, li, url };
      remove.addEventListener("click", () => removeItem(item));
      return item;
    }

    function addFiles(fileList) {
      const incoming = [...fileList].filter((f) =>
        /^(image|video|audio)\//.test(f.type)
      );
      if (!incoming.length) return;
      openSection(form, "media");
      for (const file of incoming) {
        if (items.length >= max) break;
        const item = makeCard(file);
        items.push(item);
        list.appendChild(item.li);
      }
      refreshAddState();
    }

    dropzone.addEventListener("click", () => picker.click());
    picker.addEventListener("change", () => {
      addFiles(picker.files);
      picker.value = ""; // let the same file be re-picked after removal
    });

    // Drag-and-drop anywhere on the composer. A counter avoids the flicker of
    // dragenter/dragleave firing as the pointer crosses child elements.
    let dragDepth = 0;
    form.addEventListener("dragenter", (event) => {
      if (![...event.dataTransfer.types].includes("Files")) return;
      dragDepth++;
      form.classList.add("is-dragover");
    });
    form.addEventListener("dragover", (event) => {
      if ([...event.dataTransfer.types].includes("Files")) event.preventDefault();
    });
    form.addEventListener("dragleave", () => {
      dragDepth = Math.max(0, dragDepth - 1);
      if (!dragDepth) form.classList.remove("is-dragover");
    });
    form.addEventListener("drop", (event) => {
      if (!event.dataTransfer.files.length) return;
      event.preventDefault();
      dragDepth = 0;
      form.classList.remove("is-dragover");
      addFiles(event.dataTransfer.files);
    });

    // Paste-to-attach: only when the clipboard actually carries files, so a
    // normal text paste into the textarea is untouched.
    form.addEventListener("paste", (event) => {
      const files = event.clipboardData && event.clipboardData.files;
      if (files && files.length) {
        event.preventDefault();
        addFiles(files);
      }
    });

    refreshAddState();
  }

  // The poll builder: swap the no-JS static rows for a dynamic add/remove
  // list. Starts at two empty choices (a poll needs at least two), grows one
  // row at a time up to the instance `max_options`, and keeps a minimum of two
  // — the remove button disables below that. Rows submit as `poll_options[]`,
  // exactly like the static fallback; empty ones are dropped server-side.
  function bindComposePoll(form) {
    const section = form.querySelector('[data-compose-section="poll"]');
    if (!section) return;
    const body = section.querySelector(".compose__poll-body");
    const staticRows = section.querySelector("[data-poll-static]");
    const controls = section.querySelector(".compose__poll-controls");
    if (!body || !staticRows) return;
    const max = parseInt(section.dataset.pollMax, 10) || 4;

    staticRows.remove();

    const rows = []; // { li, input, remove }

    const list = document.createElement("ul");
    list.className = "compose__poll-list";
    list.setAttribute("role", "list");

    const add = document.createElement("button");
    add.type = "button";
    add.className = "compose__poll-add";
    add.innerHTML = svgIcon(ICON_ADD);
    const addLabel = document.createElement("span");
    addLabel.textContent = form.dataset.i18nAddOption || "Add option";
    add.append(addLabel);

    // Both go before the duration/multiple controls, keeping the visual order.
    body.insertBefore(list, controls);
    body.insertBefore(add, controls);

    function refresh() {
      rows.forEach((row, i) => {
        row.input.placeholder = (
          form.dataset.i18nPollChoice || "Choice __number__"
        ).replace("__number__", String(i + 1));
        row.input.setAttribute(
          "aria-label",
          (form.dataset.i18nPollOption || "Poll option __number__").replace(
            "__number__",
            String(i + 1)
          )
        );
        row.remove.disabled = rows.length <= 2;
      });
      add.hidden = rows.length >= max;
    }

    function removeRow(row) {
      const i = rows.indexOf(row);
      if (i === -1 || rows.length <= 2) return;
      rows.splice(i, 1);
      row.li.remove();
      refresh();
    }

    function addRow(focus) {
      if (rows.length >= max) return;
      const li = document.createElement("li");
      li.className = "compose__poll-row";

      const input = document.createElement("input");
      input.type = "text";
      input.name = "poll_options[]";
      input.maxLength = 50;
      input.className = "compose__poll-input";

      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "compose__poll-remove";
      remove.innerHTML = svgIcon(ICON_REMOVE);
      remove.setAttribute(
        "aria-label",
        form.dataset.i18nRemoveOption || "Remove option"
      );

      li.append(input, remove);
      const row = { li, input, remove };
      remove.addEventListener("click", () => removeRow(row));
      rows.push(row);
      list.appendChild(li);
      refresh();
      if (focus) input.focus();
    }

    addRow(false);
    addRow(false);
    add.addEventListener("click", () => addRow(true));
  }

  // ---- Compose: group Title/Link fields --------------------------------
  //
  // A group link post needs a title (the server enforces it, gracefully). Make
  // the requirement visible up front: while the Link field holds a value, the
  // Title field is `required`, so the browser blocks the submit and focuses
  // Title instead of a server round-trip. No-JS still works — the server
  // re-renders the composer with an inline banner.
  function bindComposeGroup(form) {
    const link = form.querySelector("[data-compose-link]");
    const title = form.querySelector("[data-compose-title]");
    if (!link || !title) return;
    const sync = () => {
      title.required = link.value.trim() !== "";
    };
    link.addEventListener("input", sync);
    sync();
  }

  // ---- Compose: autocomplete suggestions ------------------------------
  //
  // As-you-type suggestions in the post body and CW field for the three
  // tokens Mastodon's composer completes: @mentions and #hashtags come from
  // the session-authenticated /web/compose/suggestions endpoint, :emoji:
  // shortcodes are filtered from the same custom-emoji catalog the picker
  // loads. JS-only by nature — without it the tokens are simply typed out in
  // full and resolved server-side on submit.
  const SUGGEST_DELAY = 200; // Mastodon throttles its suggestion fetches so
  const SUGGEST_EMOJI = 5; // ...and shows 5 emoji (4 accounts/tags come
  // capped from the server).
  const SUGGEST_SIGILS = { "@": "@", "＠": "@", "#": "#", "＃": "#", ":": ":" };

  // Mastodon's `textAtCursorMatchesToken`: the whitespace-delimited word
  // around the caret, when it starts with a trigger sigil and is at least 3
  // characters (sigil + 2).
  function suggestToken(value, caret) {
    const left = value.slice(0, caret).search(/\S+$/);
    if (left < 0) return null;
    const right = value.slice(caret).search(/\s/);
    const end = right < 0 ? value.length : caret + right;
    const word = value.slice(left, end);
    const type = SUGGEST_SIGILS[word[0]];
    if (!type || word.length < 3) return null;
    return { type, term: word.slice(1), start: left, end };
  }

  function bindSuggestField(field) {
    const panel = document.createElement("div");
    panel.className = "compose__suggest";
    panel.setAttribute("role", "listbox");
    panel.hidden = true;
    field.insertAdjacentElement("afterend", panel);

    let token = null; // what the shown suggestions were computed for
    let active = -1;
    let seq = 0; // stamps async lookups so a stale response can't render
    let timer = null;
    let controller = null;

    function hide() {
      seq++;
      panel.hidden = true;
      panel.replaceChildren();
      token = null;
      active = -1;
      if (timer) clearTimeout(timer);
      if (controller) controller.abort();
      timer = null;
      controller = null;
    }

    const options = () => [...panel.querySelectorAll("[role=option]")];

    function setActive(index) {
      const opts = options();
      if (!opts.length) return;
      active = (index + opts.length) % opts.length;
      opts.forEach((opt, i) => {
        opt.classList.toggle("is-active", i === active);
        opt.setAttribute("aria-selected", String(i === active));
      });
      opts[active].scrollIntoView({ block: "nearest" });
    }

    // Replace the token with the chosen completion plus a trailing space,
    // leaving the caret after it — Mastodon's `insertSuggestion`.
    function apply(index) {
      const opt = options()[index];
      if (!opt || !token) return;
      const completion = `${opt.dataset.completion} `;
      const before = field.value.slice(0, token.start);
      const caret = token.start + completion.length;
      field.value = before + completion + field.value.slice(token.end);
      hide();
      field.focus();
      field.setSelectionRange(caret, caret);
      // Notify the counter/autogrow listeners (the fresh token scan finds
      // only the completed word, so the panel stays shut).
      field.dispatchEvent(new Event("input", { bubbles: true }));
    }

    function option(completion, build) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "compose__suggest-option";
      button.setAttribute("role", "option");
      button.dataset.completion = completion;
      build(button);
      // Mousedown, default-prevented, selects without blurring the field.
      button.addEventListener("mousedown", (event) => {
        event.preventDefault();
        apply(options().indexOf(button));
      });
      return button;
    }

    function accountOption(item) {
      return option(`@${item.acct}`, (button) => {
        const img = document.createElement("img");
        img.className = "compose__suggest-avatar";
        img.alt = "";
        img.loading = "lazy";
        if (item.avatar) img.src = item.avatar;
        const name = document.createElement("span");
        name.className = "compose__suggest-name";
        name.textContent = item.display_name || item.acct.split("@")[0];
        const handle = document.createElement("span");
        handle.className = "compose__suggest-detail";
        handle.textContent = `@${item.acct}`;
        button.append(img, name, handle);
      });
    }

    function hashtagOption(name) {
      return option(`#${name}`, (button) => {
        const label = document.createElement("span");
        label.className = "compose__suggest-name";
        label.textContent = `#${name}`;
        button.append(label);
      });
    }

    function emojiOption(emoji) {
      return option(`:${emoji.shortcode}:`, (button) => {
        const img = document.createElement("img");
        img.className = "compose__suggest-emoji";
        img.src = emoji.url;
        img.alt = "";
        const name = document.createElement("span");
        name.className = "compose__suggest-name";
        name.textContent = `:${emoji.shortcode}:`;
        button.append(img, name);
      });
    }

    function show(built) {
      if (!built.length) {
        hide();
        return;
      }
      panel.replaceChildren(...built);
      panel.hidden = false;
      setActive(0);
    }

    async function fetchSuggestions(kind, term) {
      const stamp = ++seq;
      if (controller) controller.abort();
      controller = new AbortController();
      try {
        const res = await fetch(
          `/web/compose/suggestions?type=${kind}&q=${encodeURIComponent(term)}`,
          { headers: { Accept: "application/json" }, signal: controller.signal }
        );
        if (!res.ok) throw new Error(res.statusText);
        const items = await res.json();
        if (stamp !== seq) return; // token moved on while we were away
        show(
          kind === "accounts"
            ? items.map(accountOption)
            : items.map((item) => hashtagOption(item.name))
        );
      } catch (_err) {
        // Aborted or failed: suggestions are a bonus, never an error state.
      }
    }

    function lookup(found) {
      token = found;
      if (found.type === ":") {
        const stamp = ++seq;
        loadEmojiCatalog().then((catalog) => {
          if (stamp !== seq) return;
          const needle = found.term.toLowerCase();
          const kept = catalog
            .filter((e) => e.shortcode.toLowerCase().includes(needle))
            .slice(0, SUGGEST_EMOJI);
          show(kept.map(emojiOption));
        }, () => {});
        return;
      }
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        fetchSuggestions(found.type === "@" ? "accounts" : "hashtags", found.term);
      }, SUGGEST_DELAY);
    }

    field.addEventListener("input", () => {
      const caret = field.selectionStart ?? field.value.length;
      const found = suggestToken(field.value, caret);
      if (found) lookup(found);
      else hide();
    });

    field.addEventListener("keydown", (event) => {
      if (panel.hidden) return;
      switch (event.key) {
        case "ArrowDown":
          event.preventDefault();
          setActive(active + 1);
          break;
        case "ArrowUp":
          event.preventDefault();
          setActive(active - 1);
          break;
        case "Enter":
        case "Tab":
          // Ctrl/Cmd+Enter still submits even with the panel open.
          if (event.metaKey || event.ctrlKey) return;
          event.preventDefault();
          apply(active);
          break;
        case "Escape":
          event.preventDefault();
          hide();
          break;
        default:
      }
    });

    field.addEventListener("blur", hide);
  }

  function bindComposeSuggest(form) {
    // The same fields the emoji picker inserts into: the body, and the CW
    // (mentions don't belong there, but emoji and hashtags render in it).
    [
      form.querySelector("textarea[name=status]"),
      form.querySelector("[data-compose-spoiler]"),
    ]
      .filter(Boolean)
      .forEach(bindSuggestField);
  }

  // The composer's current attachments (JS media manager cards) as lightweight
  // descriptors for the preview: the local object-URL thumbnail and its alt.
  function composerMedia(form) {
    return [...form.querySelectorAll(".compose__attachment")].map((card) => {
      const img = card.querySelector(".compose__thumb img");
      const icon = card.querySelector(".compose__thumb");
      return {
        src: img ? img.src : null,
        icon: img ? null : icon ? icon.innerHTML : "",
        alt: card.querySelector(".compose__alt")?.value || "",
      };
    });
  }

  // Composites the composer's local thumbnails into the previewed card. Media
  // renders from the same object URLs the composer already holds, so nothing is
  // uploaded on preview (avoids churn and, for video, needless re-encoding) —
  // only the text is server-rendered, which is the drift the preview guards.
  function injectPreviewMedia(pane, media) {
    if (!media.length) return;
    const card = pane.querySelector(".status");
    if (!card || card.querySelector(".status__media")) return;
    const gallery = document.createElement("div");
    gallery.className = "status__media";
    gallery.dataset.count = String(Math.min(media.length, 4));
    for (const item of media) {
      const fig = document.createElement("figure");
      fig.className = "media";
      if (item.src) {
        const img = document.createElement("img");
        img.src = item.src;
        img.alt = item.alt;
        img.loading = "lazy";
        fig.appendChild(img);
      } else {
        fig.innerHTML = item.icon;
      }
      gallery.appendChild(fig);
    }
    const content = card.querySelector(".status__content");
    if (content) content.after(gallery);
    else card.appendChild(gallery);
  }

  // Server-side draft preview: the Preview button posts the text fields to
  // the compose endpoint with the `X-Compose-Preview` header and drops the
  // rendered card into the pane below — no page reload, so the file/poll state
  // in the composer is untouched. The files themselves aren't uploaded on
  // preview (that would re-encode video and churn storage); instead the local
  // thumbnails are composited into the rendered card, and `preview_has_media`
  // tells the server not to reject a media-only draft as blank. With JS off the
  // button is a plain submit that uploads and re-renders the whole page.
  function bindComposePreview(form) {
    const button = form.querySelector("[data-compose-preview-btn]");
    const pane = form.parentElement?.querySelector("[data-compose-preview]");
    if (!button || !pane) return;
    button.addEventListener("click", async (event) => {
      event.preventDefault();
      const media = composerMedia(form);
      // New-post composition is multipart because its no-JS path can carry
      // files. The edit endpoint is deliberately URL-encoded (it can only keep
      // or remove existing attachments), so preserve that contract for its
      // enhanced preview instead of making Save and Preview parse differently.
      const body = form.dataset.composePreviewUrlencoded !== undefined
        ? new URLSearchParams()
        : new FormData();
      for (const [key, value] of new FormData(form)) {
        if (!(value instanceof File)) body.append(key, value);
      }
      body.set("op", "preview");
      if (media.length) body.set("preview_has_media", "1");
      button.disabled = true;
      button.setAttribute("aria-busy", "true");
      try {
        const res = await fetch(form.action, {
          method: "POST",
          body,
          headers: { "X-Compose-Preview": "1" },
        });
        pane.innerHTML = await res.text();
        injectPreviewMedia(pane, media);
        pane.scrollIntoView({ behavior: "smooth", block: "nearest" });
      } catch (_err) {
        pane.textContent =
          form.dataset.i18nPreviewFailed || "Preview failed. Try again.";
      } finally {
        button.disabled = false;
        button.removeAttribute("aria-busy");
      }
    });
  }

  // The type selector owns visibility and successful form controls. Values stay
  // in their inputs when switching type, but inactive fields are not submitted.
  function bindComposeKind(form) {
    const kind = form.querySelector("[data-compose-kind]");
    if (!kind) return;
    const titleFields = form.querySelector("[data-compose-title-fields]");
    const title = form.querySelector("[data-compose-title]");
    const link = form.querySelector("[data-compose-link]");
    const body = form.querySelector("textarea[name=status]");
    const show = (element, visible) => {
      element.hidden = !visible;
      if (element.tagName === "FIELDSET") element.disabled = !visible;
    };
    const sync = () => {
      const value = kind.value;
      show(titleFields, value !== "note" || !!link);
      title.required = value !== "note" || !!link?.value.trim();
      form.querySelectorAll("[data-compose-event-fields]").forEach((el) => show(el, value === "event"));
      form.querySelectorAll("[data-compose-note-only]").forEach((el) => show(el, value === "note"));
      form.querySelectorAll("[data-compose-schedulable]").forEach((el) => show(el, value !== "event"));
      form.querySelectorAll("[data-compose-compatibility]").forEach((el) => show(el, el.dataset.composeCompatibility === value));
      body.placeholder = body.dataset[`placeholder${value[0].toUpperCase()}${value.slice(1)}`];
      form.querySelectorAll("[data-compose-title-label], [data-compose-body-label]").forEach((el) => {
        el.textContent = el.dataset[value];
        if (el.hasAttribute("data-compose-body-label")) el.classList.toggle("visually-hidden", value === "note");
      });
      body.rows = value === "article" ? 12 : 5;
      body.style.height = "auto";
      body.style.height = `${body.scrollHeight}px`;
    };
    kind.addEventListener("change", () => {
      sync();
      // A preview belongs to the previous type until requested again.
      form.parentElement?.querySelector("[data-compose-preview]")?.replaceChildren();
    });
    if (link) link.addEventListener("input", sync);
    sync();

    const mode = form.querySelector("select[name=event_join_mode]");
    const external = form.querySelector("[data-compose-external-fields]");
    const syncAttendance = () => {
      const visible = mode.value === "external";
      show(external, visible);
      external.querySelector("input").required = visible;
    };
    mode.addEventListener("change", syncAttendance);
    syncAttendance();
    bindEventTimeZones(form);
  }

  function bindEventTimeZones(form) {
    const zones = form.querySelector("[data-compose-timezone]");
    const start = form.querySelector("input[name=event_start]");
    if (!zones || !start) return;
    // Cache formatters: each option needs the offset at its own local reading.
    const formats = new Map();
    const offsetAt = (zone, instant) => {
      if (!formats.has(zone)) {
        formats.set(zone, new Intl.DateTimeFormat("en-US", { timeZone: zone, timeZoneName: "longOffset" }));
      }
      const label = formats.get(zone).formatToParts(instant).find((part) => part.type === "timeZoneName").value;
      const match = /^GMT([+-])(\d{2}):(\d{2})$/.exec(label);
      return match ? (match[1] === "-" ? -1 : 1) * (+match[2] * 60 + +match[3]) : 0;
    };
    const update = () => {
      const local = start.value ? Date.parse(`${start.value}Z`) : NaN;
      for (const option of zones.options) {
        try {
          let instant = Number.isFinite(local) ? local : Date.now();
          // Resolve the wall clock twice to account for the offset itself.
          if (Number.isFinite(local)) {
            for (let i = 0; i < 2; i++) instant = local - offsetAt(option.value, instant) * 60000;
          }
          const offset = offsetAt(option.value, instant);
          const hours = String(Math.floor(Math.abs(offset) / 60)).padStart(2, "0");
          const minutes = String(Math.abs(offset) % 60).padStart(2, "0");
          option.textContent = `(UTC${offset < 0 ? "−" : "+"}${hours}:${minutes}) ${option.value}`;
        } catch {
          // Keep the server's label if this browser lacks a particular zone.
        }
      }
    };
    start.addEventListener("change", update);
    update();
  }

  function bindCompose(root) {
    root.querySelectorAll("[data-compose]").forEach((form) => {
      if (form.dataset.bound) return;
      form.dataset.bound = "1";
      const textarea = form.querySelector("textarea[name=status]");
      const counter = form.querySelector("[data-compose-count]");
      const spoiler = form.querySelector("[data-compose-spoiler]");
      bindComposeToggles(form);
      bindComposeMenus(form);
      bindComposeCombo(form);
      bindComposeEmoji(form);
      bindComposeMedia(form);
      bindComposePoll(form);
      bindComposeGroup(form);
      bindComposeKind(form);
      bindComposeSuggest(form);
      bindComposePreview(form);
      if (!textarea) return;
      const kind = form.querySelector("[data-compose-kind]");

      // The server-rendered height (from the `rows` attribute) is the floor, so
      // autogrow never shrinks the box on load — only grows it. Without this a
      // tall empty composer would collapse to one line after hydration (CLS).
      const baseline = textarea.offsetHeight;

      const update = () => {
        if (textarea.dataset.autogrow !== undefined) {
          textarea.style.height = "auto";
          textarea.style.height = `${Math.max(textarea.scrollHeight, kind ? 0 : baseline)}px`;
        }
        if (counter) {
          const limit = kind?.value === "article"
            ? textarea.dataset.maxCharsLongForm
            : textarea.dataset.maxChars;
          const max = parseInt(limit, 10) || DEFAULT_MAX_CHARS;
          const left = max - weightedLength(spoiler ? spoiler.value : "", textarea.value);
          counter.textContent = String(left);
          counter.toggleAttribute("data-over", left < 0);
        }
      };
      textarea.addEventListener("input", update);
      if (kind) kind.addEventListener("change", update);
      if (spoiler) spoiler.addEventListener("input", update);
      textarea.addEventListener("keydown", (event) => {
        if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
          if (form.requestSubmit) form.requestSubmit();
          else form.submit();
        }
      });
      update();
    });
  }

  // ---- Posting-languages checklist: select all / none -------------------
  //
  // The buttons are JS-only conveniences over plain checkboxes, so they ship
  // hidden and are revealed here (same pattern as the WebAuthn controls).
  function bindLanguageControls(root) {
    root.querySelectorAll("[data-lang-controls]").forEach((controls) => {
      if (controls.dataset.bound) return;
      controls.dataset.bound = "1";
      controls.hidden = false;
      const boxes = () =>
        controls
          .closest("form")
          .querySelectorAll('input[type="checkbox"][name="languages[]"]');
      const setAll = (checked) => {
        boxes().forEach((box) => {
          box.checked = checked;
        });
      };
      controls
        .querySelector("[data-lang-select-all]")
        .addEventListener("click", () => setAll(true));
      controls
        .querySelector("[data-lang-select-none]")
        .addEventListener("click", () => setAll(false));
    });
  }

  // ---- Dependent settings fields ----------------------------------------
  //
  // The admin settings page carries knobs that only bite under another knob's
  // value: JPEG XL parameters while nothing encodes to JPEG XL, a CRF value
  // under a bitrate-budget rate control, the eleven rate-limit counts while
  // rate limiting is off. A field (or a wrapper around several) declares its
  // condition in `data-show-when`; while it does not hold, the field is
  // hidden — not disabled, so its stored value still round-trips through a
  // save. With JS off nothing is hidden and every field stays editable, which
  // is the whole no-JS contract: this is decluttering, never gating.
  //
  // Grammar: `a|b|c` — terms OR'd. A term is `name` (a checkbox that is
  // checked, or any other control with a non-empty value), `name=value`, or
  // `name!=value`. A term naming a control that is itself hidden is false, so
  // a chain (profile directory → anonymous directory → its remote scope)
  // collapses whole. Controls are looked up across the page, not per form, so
  // a condition may point at a knob living in another collapsed group.
  function settingsControlValue(scope, name) {
    const control = scope.querySelector(`[name="${name}"]`);
    if (!control || control.closest(".is-collapsed")) return null;
    if (control.type === "checkbox" || control.type === "radio") {
      return control.checked ? control.value || "1" : "";
    }
    return control.value;
  }

  function settingsTermHolds(scope, term) {
    const split = term.indexOf("!=") >= 0 ? term.indexOf("!=") : term.indexOf("=");
    if (split < 0) return Boolean(settingsControlValue(scope, term));
    const negated = term[split] === "!";
    const value = settingsControlValue(scope, term.slice(0, split));
    if (value === null) return false;
    const wanted = term.slice(split + (negated ? 2 : 1));
    return negated ? value !== wanted : value === wanted;
  }

  function bindSettingsDeps(root) {
    root.querySelectorAll("[data-settings-deps]").forEach((scope) => {
      if (scope.dataset.bound) return;
      scope.dataset.bound = "1";
      // Document order: a governing control is always rendered before the
      // fields that depend on it, so one pass settles a chain.
      const dependents = [...scope.querySelectorAll("[data-show-when]")];
      if (!dependents.length) return;
      const apply = () => {
        dependents.forEach((field) => {
          const shown = field.dataset.showWhen
            .split("|")
            .some((term) => settingsTermHolds(scope, term.trim()));
          field.classList.toggle("is-collapsed", !shown);
        });
      };
      scope.addEventListener("change", apply);
      scope.addEventListener("input", apply);
      apply();
    });
  }

  // ---- Infinite scroll --------------------------------------------------
  //
  // Applies to any page that marks its repeating-items container with
  // `data-paged` and renders a pager (`.pager` or `.admin-pager`) whose link
  // fetches the next page: timelines, the profile media wall, follow and
  // engagement lists, notifications, the relationships manager, list members,
  // admin tables. One paged region per page.
  //
  // Preloads one page ahead of the reader. We track the *first* item of the
  // currently-last page (the "sentinel") and, on every scroll/resize frame,
  // test its position directly: the moment its top edge rises to or above the
  // viewport bottom — i.e. the last page has become visible — we fetch the
  // next page, append its items, and advance the sentinel to the first of
  // those. Because the load fires while the whole last page is still below the
  // fold, a full page stays buffered ahead and fast scrolling doesn't stutter
  // at the seam.
  //
  // Testing the sentinel's geometry each frame — rather than reacting to an
  // IntersectionObserver *toggle* — is deliberate. An observer only fires when
  // intersection flips, so jumping the scrollbar (or End / Page Down / an
  // anchor) straight past the sentinel takes it from below the fold to above
  // the fold without ever "intersecting": state goes false → false, no
  // callback fires, and the reader is stranded at the bottom with nothing
  // loading until they scroll back up into it. The direct half-plane test
  // (`top <= innerHeight`) fires whenever the last page is at or above the
  // viewport bottom, no matter how the reader got there, and re-checks after
  // each append so a big jump keeps pulling pages until the feed once again
  // extends below the fold.
  //
  // On first render the sentinel is the top of the initial page, already on
  // screen, so page two is fetched straight away. The "Load older" pager link
  // stays in the DOM and clickable as the no-JS / fallback path.
  //
  // Browsers can restore the entire expanded DOM from the back/forward cache,
  // but that cache is deliberately best-effort. For a normal history reload,
  // remember which cursor pages were appended and the first visible post.
  // Replaying those cursors before restoring the post keeps Back reliable even
  // when the browser has discarded the old document.
  let sentinel = null; // first item of the last loaded page, or null when exhausted
  let checkScheduled = false;
  let loadedPageUrls = [];

  const pagerHistoryKey = "plamenuPager";

  function pagerLocation() {
    return `${window.location.pathname}${window.location.search}`;
  }

  function localPagerUrl(value) {
    try {
      const url = new URL(value, window.location.href);
      if (url.origin !== window.location.origin) return null;
      return `${url.pathname}${url.search}`;
    } catch (_err) {
      return null;
    }
  }

  function savedPagerHistory() {
    const saved = history.state?.[pagerHistoryKey];
    if (
      !saved ||
      saved.location !== pagerLocation() ||
      !Array.isArray(saved.pages)
    ) return null;
    return saved;
  }

  function visiblePagedAnchor(container) {
    // Feed cards have stable post ids, including cards nested inside a thread
    // group. Other paged surfaces may put an id on their direct children.
    const candidates = container.querySelectorAll(
      "article.status[id], :scope > [id]",
    );
    for (const candidate of candidates) {
      const rect = candidate.getBoundingClientRect();
      if (rect.bottom > 0 && rect.top < window.innerHeight) {
        return { id: candidate.id, offset: rect.top };
      }
    }
    return null;
  }

  function savePagerHistory() {
    const container = document.querySelector("[data-paged]");
    if (!container) return;
    const current = history.state;
    const state = current && typeof current === "object" ? current : {};
    history.replaceState(
      {
        ...state,
        [pagerHistoryKey]: {
          location: pagerLocation(),
          pages: loadedPageUrls,
          scrollY: window.scrollY,
          anchor: visiblePagedAnchor(container),
        },
      },
      "",
      window.location.href,
    );
  }

  function restorePagerPosition(container, saved) {
    const apply = () => {
      const anchor = saved.anchor;
      const target = anchor?.id ? document.getElementById(anchor.id) : null;
      if (target && container.contains(target) && Number.isFinite(anchor.offset)) {
        const top = window.scrollY + target.getBoundingClientRect().top;
        window.scrollTo(0, top - anchor.offset);
      } else if (Number.isFinite(saved.scrollY)) {
        window.scrollTo(0, saved.scrollY);
      }
    };
    // Native history restoration runs around `pageshow`. Apply our position
    // one frame later so a scroll offset clamped against the short first page
    // cannot overwrite the restored card after its older pages are replayed.
    return new Promise((resolve) => requestAnimationFrame(() => {
      apply();
      requestAnimationFrame(() => {
        apply();
        resolve();
      });
    }));
  }

  async function watchPager(root) {
    const container = root.querySelector("[data-paged]");
    if (!container || !root.querySelector(".pager, .admin-pager")) return;
    if (container.dataset.pagerBound) return;
    container.dataset.pagerBound = "1";
    sentinel = container.firstElementChild || null;
    const saved = savedPagerHistory();
    // TODO FIXME: restorePagerPosition disabled due to implementation
    // being naive (can trigger rapid load of any amount of pages that
    // user scrolled through) and unreliable (can trigger on any
    // page load, not just `back` action). Does more harm than good
    // in the current state. Preferably repair the bfcache issue.
    // This comment being there means bfcache issue is still not fixed.
    // If not possible, rework whatever this is into loading
    // the page that contains last seen post, either automatically with
    // notice for the user and only on an actual `back` navigation,
    // or as non-obtrusive button to jump to last seen page.
    if (false && saved) {
      // `pageshow` follows the browser's own scroll restoration. Start
      // listening before replaying pages so even a fast load cannot beat us.
      const shown = document.readyState === "complete"
        ? Promise.resolve()
        : new Promise((resolve) => {
          window.addEventListener("pageshow", resolve, { once: true });
        });
      loadedPageUrls = [];
      for (const value of saved.pages) {
        const url = localPagerUrl(value);
        if (!url || !(await loadMore(url, true))) break;
        loadedPageUrls.push(url);
      }
      await shown;
      await restorePagerPosition(container, saved);
    }
    window.addEventListener("scroll", scheduleCheck, { passive: true });
    window.addEventListener("resize", scheduleCheck, { passive: true });
    window.addEventListener("pagehide", savePagerHistory);
    savePagerHistory();
    scheduleCheck();
  }

  // Coalesce the flood of scroll/resize events into one geometry test per frame.
  function scheduleCheck() {
    if (checkScheduled) return;
    checkScheduled = true;
    requestAnimationFrame(() => {
      checkScheduled = false;
      maybeLoadMore();
    });
  }

  function maybeLoadMore() {
    if (!sentinel) return;
    // Fire once the last page's top edge is at or above the viewport bottom —
    // true whether the reader reached its top, jumped into its middle, or
    // slammed to the very end.
    if (sentinel.getBoundingClientRect().top <= window.innerHeight) {
      loadMore();
    }
  }

  async function loadMore(savedUrl = null, restoring = false) {
    const pager = document.querySelector(".pager, .admin-pager");
    const link = pager?.querySelector("a");
    if (!link || pager.dataset.loading) return false;
    const requestUrl = localPagerUrl(savedUrl || link.href);
    if (!requestUrl) return false;
    pager.dataset.loading = "1";
    try {
      const res = await fetch(requestUrl, {
        headers: { "X-Requested-With": "fetch" },
      });
      if (!res.ok) throw new Error(res.statusText);
      const doc = new DOMParser().parseFromString(await res.text(), "text/html");
      const container = document.querySelector("[data-paged]");
      const incoming = doc.querySelector("[data-paged]");
      let firstNew = null;
      if (container && incoming) {
        [...incoming.children].forEach((node) => {
          container.appendChild(node);
          firstNew ||= node;
        });
        // Re-scan the whole container rather than each appended node: some
        // enhanced elements (media-wall tiles) *are* the appended node, and
        // querySelectorAll never matches its own root. Every binder is
        // idempotent (dataset guards), so the re-scan is safe.
        enhance(container);
      }
      const nextPager = doc.querySelector(".pager, .admin-pager");
      if (nextPager) {
        pager.replaceWith(nextPager);
      } else {
        pager.remove();
      }
      // Advance the sentinel to the start of the page we just appended, so the
      // next fetch fires when the reader reaches it. No next pager means the
      // feed is exhausted — stop watching.
      sentinel = nextPager ? firstNew : null;
      if (!restoring) {
        loadedPageUrls.push(requestUrl);
        savePagerHistory();
      }
      // A large jump can leave the new sentinel still at or above the fold;
      // re-check so we keep pulling pages until the feed extends below it again.
      if (!restoring) maybeLoadMore();
      return true;
    } catch (_err) {
      delete pager.dataset.loading; // let a manual click retry
      return false;
    }
  }

  // ---- WebAuthn security keys ------------------------------------------
  //
  // The only part of the UI that genuinely needs JavaScript: WebAuthn's
  // ceremonies run in the browser. The TOTP form is always the no-JS fallback,
  // so these controls start `hidden` in the markup and are revealed here only
  // when the platform supports `navigator.credentials`. Binary fields cross the
  // wire as base64url strings (matching the server's serialization); the shim
  // converts them to/from ArrayBuffers around the WebAuthn calls.
  function b64urlToBuf(value) {
    const pad = value.length % 4 === 0 ? "" : "=".repeat(4 - (value.length % 4));
    const base64 = (value + pad).replace(/-/g, "+").replace(/_/g, "/");
    const binary = atob(base64);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes.buffer;
  }

  function bufToB64url(buffer) {
    let binary = "";
    for (const byte of new Uint8Array(buffer)) binary += String.fromCharCode(byte);
    return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  function prepareCreation(publicKey) {
    publicKey.challenge = b64urlToBuf(publicKey.challenge);
    publicKey.user.id = b64urlToBuf(publicKey.user.id);
    if (publicKey.excludeCredentials) {
      publicKey.excludeCredentials = publicKey.excludeCredentials.map((c) => ({
        ...c,
        id: b64urlToBuf(c.id),
      }));
    }
    return publicKey;
  }

  function prepareRequest(publicKey) {
    publicKey.challenge = b64urlToBuf(publicKey.challenge);
    if (publicKey.allowCredentials) {
      publicKey.allowCredentials = publicKey.allowCredentials.map((c) => ({
        ...c,
        id: b64urlToBuf(c.id),
      }));
    }
    return publicKey;
  }

  function encodeAttestation(cred) {
    return {
      id: cred.id,
      rawId: bufToB64url(cred.rawId),
      type: cred.type,
      response: {
        attestationObject: bufToB64url(cred.response.attestationObject),
        clientDataJSON: bufToB64url(cred.response.clientDataJSON),
      },
      clientExtensionResults: cred.getClientExtensionResults(),
    };
  }

  function encodeAssertion(cred) {
    const r = cred.response;
    return {
      id: cred.id,
      rawId: bufToB64url(cred.rawId),
      type: cred.type,
      response: {
        authenticatorData: bufToB64url(r.authenticatorData),
        clientDataJSON: bufToB64url(r.clientDataJSON),
        signature: bufToB64url(r.signature),
        userHandle: r.userHandle ? bufToB64url(r.userHandle) : null,
      },
      clientExtensionResults: cred.getClientExtensionResults(),
    };
  }

  function setStatus(el, text) {
    if (el) el.textContent = text;
  }

  async function errorText(res) {
    try {
      const data = await res.json();
      return data.error || res.statusText;
    } catch (_e) {
      return res.statusText;
    }
  }

  // The shim has no message catalog; the server hangs the localized wording
  // off the container it drives (`data-webauthn-*`), the way the push settings
  // do.
  function webauthnText(root, name) {
    return (root && root.dataset[name]) || "";
  }

  function friendlyError(root, err) {
    if (err && (err.name === "NotAllowedError" || err.name === "AbortError")) {
      return webauthnText(root, "webauthnDismissed");
    }
    return (err && err.message) || webauthnText(root, "webauthnFailed");
  }

  async function registerSecurityKey(button) {
    const form = button.closest("[data-webauthn-register-form]");
    const status = form.querySelector("[data-webauthn-status]");
    const nickname = form.querySelector("[data-webauthn-nickname]").value.trim();
    const csrf = button.dataset.webauthnCsrf;
    if (!nickname) {
      setStatus(status, webauthnText(form, "webauthnNameFirst"));
      return;
    }
    button.disabled = true;
    setStatus(status, webauthnText(form, "webauthnPrompt"));
    try {
      const optRes = await fetch("/web/settings/webauthn/options", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ csrf, nickname }),
      });
      if (!optRes.ok) throw new Error(await errorText(optRes));
      const { challenge_token, options } = await optRes.json();
      const credential = await navigator.credentials.create({
        publicKey: prepareCreation(options.publicKey),
      });
      const finishRes = await fetch("/web/settings/webauthn", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          csrf,
          challenge_token,
          credential: encodeAttestation(credential),
        }),
      });
      if (!finishRes.ok) throw new Error(await errorText(finishRes));
      window.location.reload();
    } catch (err) {
      button.disabled = false;
      setStatus(status, friendlyError(form, err));
    }
  }

  async function authenticateSecurityKey(button) {
    const container = button.closest("[data-webauthn-login]");
    const status = container.querySelector("[data-webauthn-status]");
    const token = button.dataset.webauthnToken;
    // The first-party sign-in posts to /login/webauthn*; the OAuth 2FA prompt
    // overrides these to its own sessionless endpoints via data attributes.
    const optionsUrl =
      container.dataset.webauthnOptionsUrl || "/login/webauthn/options";
    const finishUrl = container.dataset.webauthnFinishUrl || "/login/webauthn";
    button.disabled = true;
    setStatus(status, webauthnText(container, "webauthnPrompt"));
    try {
      const optRes = await fetch(optionsUrl, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ challenge_token: token }),
      });
      if (!optRes.ok) throw new Error(await errorText(optRes));
      const { options } = await optRes.json();
      const assertion = await navigator.credentials.get({
        publicKey: prepareRequest(options.publicKey),
      });
      // Carry along any hidden fields from the adjacent form — the OAuth flow
      // needs its authorization-request params to mint the grant; web sign-in
      // adds nothing beyond challenge_token (already present).
      const body = {
        challenge_token: token,
        credential: encodeAssertion(assertion),
      };
      const form = container
        .closest(".auth-card")
        ?.querySelector("form.auth-form");
      if (form) {
        form.querySelectorAll("input[type=hidden]").forEach((input) => {
          if (input.name && !(input.name in body)) body[input.name] = input.value;
        });
      }
      const finishRes = await fetch(finishUrl, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!finishRes.ok) throw new Error(await errorText(finishRes));
      const data = await finishRes.json();
      // OAuth out-of-band clients have no redirect target — show the code.
      if (data.oob_code) {
        setStatus(
          status,
          webauthnText(container, "webauthnOob").replace(
            "{code}",
            data.oob_code,
          ),
        );
        button.disabled = false;
        return;
      }
      window.location.assign(data.redirect || "/");
    } catch (err) {
      button.disabled = false;
      setStatus(status, friendlyError(container, err));
    }
  }

  function bindWebauthn(root) {
    const supported = !!window.PublicKeyCredential;
    // Reveal the JS-only controls only where the platform can drive them.
    root
      .querySelectorAll("[data-webauthn-login], [data-webauthn-register-form]")
      .forEach((el) => {
        if (supported) el.hidden = false;
      });
    if (!supported) return;
    root.querySelectorAll("[data-webauthn-register]").forEach((btn) => {
      if (btn.dataset.bound) return;
      btn.dataset.bound = "1";
      btn.addEventListener("click", () => registerSecurityKey(btn));
    });
    root.querySelectorAll("[data-webauthn-authenticate]").forEach((btn) => {
      if (btn.dataset.bound) return;
      btn.dataset.bound = "1";
      btn.addEventListener("click", () => authenticateSecurityKey(btn));
    });
  }

  // ---- Media lightbox -------------------------------------------------
  //
  // Image and gifv thumbnails link to the raw file in a new tab (the no-JS
  // behavior); here a plain click opens an in-page viewer instead: prev/next
  // across the post's media, wheel/pinch zoom with drag panning, swipe
  // navigation, alt text shown, focus-trapped dialog. A plain click or tap
  // anywhere outside the controls closes it.
  let lightbox = null;

  function ensureLightbox() {
    if (lightbox) return lightbox;
    const el = document.createElement("div");
    el.className = "lightbox";
    el.hidden = true;
    el.setAttribute("role", "dialog");
    el.setAttribute("aria-modal", "true");
    el.setAttribute("aria-label", "Media viewer");
    el.innerHTML = `
      <div class="lightbox__stage">
        <div class="lightbox__track">
          <div class="lightbox__slide" aria-hidden="true">
            <img class="lightbox__media lightbox__media--img" alt="" hidden>
            <video class="lightbox__media lightbox__media--video" muted loop playsinline preload="metadata" hidden></video>
          </div>
          <div class="lightbox__slide">
            <img class="lightbox__media lightbox__media--img" alt="" hidden>
            <video class="lightbox__media lightbox__media--video" muted loop playsinline preload="metadata" hidden></video>
          </div>
          <div class="lightbox__slide" aria-hidden="true">
            <img class="lightbox__media lightbox__media--img" alt="" hidden>
            <video class="lightbox__media lightbox__media--video" muted loop playsinline preload="metadata" hidden></video>
          </div>
        </div>
      </div>
      <div class="lightbox__meta">
        <span class="lightbox__counter"></span>
        <p class="lightbox__alt"></p>
        <button type="button" class="lightbox__alt-toggle" aria-expanded="false" hidden>Show more</button>
        <a class="lightbox__post" hidden>View post</a>
      </div>
      <button type="button" class="lightbox__button lightbox__close" aria-label="Close">
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18"></path></svg>
      </button>
      <button type="button" class="lightbox__button lightbox__nav lightbox__nav--prev" aria-label="Previous">
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="m15 5-7 7 7 7"></path></svg>
      </button>
      <button type="button" class="lightbox__button lightbox__nav lightbox__nav--next" aria-label="Next">
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="m9 5 7 7-7 7"></path></svg>
      </button>`;
    document.body.append(el);

    const box = {
      el,
      stage: el.querySelector(".lightbox__stage"),
      track: el.querySelector(".lightbox__track"),
      img: null,
      video: null,
      counter: el.querySelector(".lightbox__counter"),
      alt: el.querySelector(".lightbox__alt"),
      altToggle: el.querySelector(".lightbox__alt-toggle"),
      post: el.querySelector(".lightbox__post"),
      close: el.querySelector(".lightbox__close"),
      prev: el.querySelector(".lightbox__nav--prev"),
      next: el.querySelector(".lightbox__nav--next"),
      items: [],
      index: 0,
      opener: null,
      // The visible element (img or video) that zoom/pan transforms apply to,
      // translated by (tx, ty) then scaled.
      media: null,
      scale: 1,
      tx: 0,
      ty: 0,
      dragX: 0,
      animating: false,
      animationId: 0,
      // When a drag/swipe/pinch last ended, so the click the browser may
      // synthesize right after it doesn't count as a closing tap.
      gestureAt: 0,
      // Opening the viewer adds one same-page history entry. This lets the
      // browser Back gesture close the viewer before leaving the page.
      historyActive: false,
    };
    box.prev.addEventListener("click", (event) => {
      event.stopPropagation();
      moveLightbox(-1);
    });
    box.next.addEventListener("click", (event) => {
      event.stopPropagation();
      moveLightbox(1);
    });
    // The backdrop ignores the synthetic click after a gesture. Give Close
    // its own path so a user can dismiss immediately after swiping instead of
    // being caught by that short fall-through guard.
    box.close.addEventListener("click", (event) => {
      event.stopPropagation();
      closeLightbox();
    });
    box.altToggle.addEventListener("click", () => {
      setLightboxAltExpanded(!box.alt.classList.contains("is-expanded"));
    });
    // Closing happens on click, not pointerup: the overlay is still visible
    // when the browser synthesizes the click, so a tap can't fall through to
    // whatever the page has underneath. The nav/meta areas opt out above.
    el.addEventListener("click", (event) => {
      if (Date.now() - box.gestureAt < 400) return;
      if (event.target.closest(".lightbox__meta")) return;
      closeLightbox();
    });
    bindLightboxGestures(box);
    window.addEventListener("popstate", () => {
      if (!box.historyActive) return;
      box.historyActive = false;
      closeLightbox(true);
    });
    lightbox = box;
    return box;
  }

  function openLightbox(items, index, opener) {
    if (!Array.isArray(items) || items.length === 0) return;
    const box = ensureLightbox();
    box.items = items;
    box.opener = opener;
    if (!box.historyActive) {
      history.pushState(history.state, "", location.href);
      box.historyActive = true;
    }
    box.el.hidden = false;
    document.body.style.overflow = "hidden";
    document.addEventListener("keydown", onLightboxKey, true);
    showLightboxItem(
      Number.isInteger(index) && index >= 0 && index < items.length ? index : 0,
    );
    box.close.focus();
  }

  function closeLightbox(fromHistory = false) {
    const box = lightbox;
    if (!box || box.el.hidden) return;
    box.el.hidden = true;
    box.track.querySelectorAll("video").forEach((video) => {
      video.pause();
      video.removeAttribute("src");
    });
    box.track.querySelectorAll("img").forEach((img) => img.removeAttribute("src"));
    box.track.classList.remove("is-animating");
    box.track.style.transform = "";
    box.animating = false;
    box.animationId += 1;
    [...box.track.children].forEach((slide) => {
      slide.dataset.itemKey = "";
    });
    document.body.style.overflow = "";
    document.removeEventListener("keydown", onLightboxKey, true);
    if (box.opener) box.opener.focus();
    box.opener = null;
    // Button, backdrop and Escape closes must remove the entry that opening
    // added. A popstate close is already moving past that entry.
    if (box.historyActive && !fromHistory) {
      box.historyActive = false;
      history.back();
    }
  }

  function showLightboxItem(index) {
    const box = lightbox;
    if (!Number.isInteger(index) || index < 0 || index >= box.items.length) {
      return false;
    }
    box.index = index;
    box.animationId += 1;
    box.animating = false;
    box.dragX = 0;
    box.track.classList.remove("is-animating");
    box.track.style.transform = "";
    renderLightboxSlides();
    resetLightboxZoom();
    updateLightboxDetails();
    return true;
  }

  // Keep only the current item and its two neighbours alive. The three fixed
  // slides mean an adjacent image is already on-screen while a finger drags,
  // without eagerly loading an entire profile media wall.
  function renderLightboxSlides() {
    const box = lightbox;
    [...box.track.children].forEach((slide, slot) => {
      const itemIndex = box.index + slot - 1;
      const item = box.items[itemIndex];
      const current = slot === 1;
      const img = slide.querySelector("img");
      const video = slide.querySelector("video");
      slide.setAttribute("aria-hidden", String(!current));

      if (!item) {
        slide.dataset.itemKey = "";
        img.hidden = true;
        img.removeAttribute("src");
        img.alt = "";
        video.hidden = true;
        video.pause();
        video.removeAttribute("src");
        video.removeAttribute("title");
        return;
      }

      const itemKey = `${itemIndex}:${item.type}:${item.url}`;
      if (slide.dataset.itemKey !== itemKey) {
        slide.dataset.itemKey = itemKey;
        img.removeAttribute("src");
        video.pause();
        video.removeAttribute("src");
        if (item.type === "gifv") {
          img.hidden = true;
          img.alt = "";
          video.hidden = false;
          video.src = item.url;
          video.title = item.alt;
        } else {
          video.hidden = true;
          video.removeAttribute("title");
          img.hidden = false;
          img.src = item.url;
          img.alt = current ? item.alt : "";
        }
      } else if (!img.hidden) {
        // A neighbour becomes the accessible current slide after rotation.
        img.alt = current ? item.alt : "";
      }

      if (current) {
        box.img = img;
        box.video = video;
        box.media = item.type === "gifv" ? video : img;
        if (item.type === "gifv") video.play().catch(() => {});
      } else {
        video.pause();
      }
    });
  }

  function updateLightboxDetails() {
    const box = lightbox;
    const index = box.index;
    const item = box.items[index];
    box.alt.textContent = item.alt;
    setLightboxAltExpanded(false);
    // The toggle only appears when the collapsed clamp actually cut the
    // description off. The dialog is visible by the time items are shown,
    // so the overflow measurement is real.
    box.altToggle.hidden = box.alt.scrollHeight <= box.alt.clientHeight;
    // Media-wall items carry their containing post; in-post galleries don't
    // need the link (the post is right behind the dialog).
    box.post.hidden = !item.post;
    if (item.post) box.post.href = item.post;
    box.counter.textContent =
      box.items.length <= 1 ? "" : `${index + 1} / ${box.items.length}`;
    // A control is present only when it can succeed. This keeps pointer,
    // keyboard and announced state derived from the same bounds check.
    box.prev.hidden = index === 0;
    box.next.hidden = index === box.items.length - 1;
    box.prev.disabled = box.prev.hidden;
    box.next.disabled = box.next.hidden;
  }

  function moveLightbox(direction, dragged = false) {
    const box = lightbox;
    const nextIndex = box.index + direction;
    if (
      box.animating ||
      !Number.isInteger(nextIndex) ||
      nextIndex < 0 ||
      nextIndex >= box.items.length
    ) {
      settleLightboxTrack();
      return false;
    }
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      showLightboxItem(nextIndex);
      return true;
    }

    resetLightboxZoom();
    box.animating = true;
    const animationId = ++box.animationId;
    box.track.classList.add("is-animating");
    // A dragged track already has an inline pixel offset. Force that frame to
    // be the transition's start before sending it to the neighbouring slot.
    if (dragged) void box.track.offsetWidth;
    box.track.style.transform =
      direction > 0 ? "translate3d(-200%, 0, 0)" : "translate3d(0%, 0, 0)";

    let finished = false;
    const finish = () => {
      if (finished) return;
      finished = true;
      box.track.removeEventListener("transitionend", onEnd);
      if (animationId !== box.animationId) return;
      box.track.classList.remove("is-animating");
      box.index = nextIndex;
      if (direction > 0) box.track.append(box.track.firstElementChild);
      else box.track.prepend(box.track.lastElementChild);
      box.track.style.transform = "";
      box.dragX = 0;
      box.animating = false;
      renderLightboxSlides();
      resetLightboxZoom();
      updateLightboxDetails();
    };
    const onEnd = (event) => {
      if (event.target === box.track && event.propertyName === "transform") finish();
    };
    box.track.addEventListener("transitionend", onEnd);
    window.setTimeout(finish, 280);
    return true;
  }

  function settleLightboxTrack() {
    const box = lightbox;
    if (!box || box.animating) return;
    const animationId = ++box.animationId;
    box.dragX = 0;
    box.animating = true;
    box.track.classList.add("is-animating");
    box.track.style.transform = "";
    window.setTimeout(() => {
      if (animationId !== box.animationId) return;
      box.track.classList.remove("is-animating");
      box.animating = false;
      renderLightboxSlides();
    }, 240);
  }

  function setLightboxAltExpanded(expanded) {
    const box = lightbox;
    box.alt.classList.toggle("is-expanded", expanded);
    box.alt.scrollTop = 0;
    box.altToggle.textContent = expanded ? "Show less" : "Show more";
    box.altToggle.setAttribute("aria-expanded", String(expanded));
  }

  function onLightboxKey(event) {
    const box = lightbox;
    if (event.key === "Escape") {
      event.preventDefault();
      closeLightbox();
    } else if (event.key === "ArrowLeft") {
      moveLightbox(-1);
    } else if (event.key === "ArrowRight") {
      moveLightbox(1);
    } else if (event.key === "Tab") {
      // Focus trap: cycle through the dialog's buttons and the post link.
      const buttons = [...box.el.querySelectorAll("button:not([hidden]), a:not([hidden])")];
      const first = buttons[0];
      const last = buttons[buttons.length - 1];
      if (!box.el.contains(document.activeElement)) {
        event.preventDefault();
        first.focus();
      } else if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    }
  }

  function resetLightboxZoom() {
    const box = lightbox;
    box.scale = 1;
    box.tx = 0;
    box.ty = 0;
    for (const media of box.track.querySelectorAll(".lightbox__media")) {
      media.style.transform = "";
      media.classList.remove("is-zoomed");
    }
  }

  function applyLightboxTransform() {
    const box = lightbox;
    box.media.style.transform =
      box.scale === 1 && !box.tx && !box.ty
        ? ""
        : `translate(${box.tx}px, ${box.ty}px) scale(${box.scale})`;
    box.media.classList.toggle("is-zoomed", box.scale > 1);
  }

  // Keep the pan within the media's overflow so it can't be dragged fully
  // off-screen.
  function clampLightboxPan() {
    const box = lightbox;
    const maxX = Math.max(0, (box.media.offsetWidth * box.scale - box.stage.clientWidth) / 2);
    const maxY = Math.max(0, (box.media.offsetHeight * box.scale - box.stage.clientHeight) / 2);
    box.tx = Math.min(maxX, Math.max(-maxX, box.tx));
    box.ty = Math.min(maxY, Math.max(-maxY, box.ty));
  }

  // Rescale around a viewport point so the pixel under the cursor/fingers
  // stays put: the translation shrinks/grows with the scale ratio.
  function zoomLightboxAround(nextScale, clientX, clientY) {
    const box = lightbox;
    const clamped = Math.min(8, Math.max(1, nextScale));
    const rect = box.stage.getBoundingClientRect();
    const px = clientX - rect.left - rect.width / 2;
    const py = clientY - rect.top - rect.height / 2;
    const ratio = clamped / box.scale;
    box.tx = px - (px - box.tx) * ratio;
    box.ty = py - (py - box.ty) * ratio;
    box.scale = clamped;
    if (box.scale === 1) {
      box.tx = 0;
      box.ty = 0;
    }
    clampLightboxPan();
    applyLightboxTransform();
  }

  function bindLightboxGestures(box) {
    box.stage.addEventListener(
      "wheel",
      (event) => {
        event.preventDefault();
        const factor = event.deltaY < 0 ? 1.2 : 1 / 1.2;
        zoomLightboxAround(box.scale * factor, event.clientX, event.clientY);
      },
      { passive: false },
    );

    // Pointer events cover mouse drag, touch swipe and two-finger pinch
    // uniformly. One pointer pans when zoomed or swipes to navigate at rest;
    // two pointers pinch-zoom around their midpoint. Plain clicks/taps are
    // NOT handled here — pointer capture retargets pointerup to the stage,
    // and closing before the browser synthesizes the click would let a tap
    // fall through to the page. The dialog's click listener does the closing.
    const pointers = new Map();
    let moved = false;
    box.stage.addEventListener("pointerdown", (event) => {
      if (box.animating) {
        event.preventDefault();
        return;
      }
      pointers.set(event.pointerId, {
        x: event.clientX,
        y: event.clientY,
        startX: event.clientX,
        startY: event.clientY,
        startedAt: Date.now(),
        axis: null,
        pinched: false,
      });
      moved = false;
      if (pointers.size === 2) {
        box.dragX = 0;
        box.track.classList.remove("is-animating");
        box.track.style.transform = "";
        pointers.forEach((pointer) => {
          pointer.pinched = true;
        });
      } else if (box.scale === 1) {
        // Let a gifv neighbour animate as it comes under the finger.
        box.track.querySelectorAll("video:not([hidden])").forEach((video) => {
          video.play().catch(() => {});
        });
      }
      box.stage.setPointerCapture(event.pointerId);
      event.preventDefault();
    });
    box.stage.addEventListener("pointermove", (event) => {
      const p = pointers.get(event.pointerId);
      if (!p) return;
      const dx = event.clientX - p.x;
      const dy = event.clientY - p.y;
      const totalX = event.clientX - p.startX;
      const totalY = event.clientY - p.startY;
      if (Math.abs(totalX) + Math.abs(totalY) > 8) {
        moved = true;
        if (!p.axis) p.axis = Math.abs(totalX) > Math.abs(totalY) ? "x" : "y";
      }
      if (pointers.size === 2) {
        box.gestureAt = Date.now();
        pointers.forEach((pointer) => {
          pointer.pinched = true;
        });
        const [a, b] = [...pointers.values()];
        const before = Math.hypot(a.x - b.x, a.y - b.y);
        p.x = event.clientX;
        p.y = event.clientY;
        const after = Math.hypot(a.x - b.x, a.y - b.y);
        if (before > 0) {
          zoomLightboxAround(
            box.scale * (after / before),
            (a.x + b.x) / 2,
            (a.y + b.y) / 2,
          );
        }
        return;
      }
      p.x = event.clientX;
      p.y = event.clientY;
      if (box.scale > 1) {
        box.tx += dx;
        box.ty += dy;
        clampLightboxPan();
        applyLightboxTransform();
      } else if (p.axis === "x" && !p.pinched) {
        const atStart = totalX > 0 && box.index === 0;
        const atEnd = totalX < 0 && box.index === box.items.length - 1;
        box.dragX = atStart || atEnd ? totalX * 0.22 : totalX;
        box.track.style.transform = `translate3d(calc(-100% + ${box.dragX}px), 0, 0)`;
        box.gestureAt = Date.now();
      }
    });
    const release = (event) => {
      const p = pointers.get(event.pointerId);
      pointers.delete(event.pointerId);
      if (!p) return;
      if (moved) box.gestureAt = Date.now();
      if (pointers.size > 0) return;
      const swipe = event.clientX - p.startX;
      const elapsed = Math.max(1, Date.now() - p.startedAt);
      const fast = Math.abs(swipe) > 24 && Math.abs(swipe) / elapsed > 0.35;
      const far = Math.abs(swipe) > Math.min(88, box.stage.clientWidth * 0.18);
      if (
        event.type === "pointerup" &&
        box.scale === 1 &&
        moved &&
        p.axis === "x" &&
        !p.pinched &&
        (fast || far)
      ) {
        // The track is already following the pointer; finish the same motion
        // into the adjacent slot, or spring back if this is an outer edge.
        moveLightbox(swipe < 0 ? 1 : -1, true);
      } else if (box.scale === 1) {
        settleLightboxTrack();
      } else {
        renderLightboxSlides();
      }
    };
    box.stage.addEventListener("pointerup", release);
    box.stage.addEventListener("pointercancel", release);
  }

  // The lightbox entry for one gallery tile: an image/gifv link, the
  // autoplaying gifv <video> the auto-play preference renders instead, or a
  // profile media-wall tile (which links to the post and carries the media
  // file in data-media-url).
  function lightboxItemOf(tile) {
    if (tile.matches("video.media__gifv")) {
      return { type: "gifv", url: tile.src, alt: tile.title || "" };
    }
    return {
      type: tile.dataset.gifv !== undefined ? "gifv" : "image",
      url: tile.dataset.mediaUrl || tile.href,
      alt: tile.querySelector("img")?.alt || "",
      post: tile.dataset.mediaUrl ? tile.href : "",
    };
  }

  const LIGHTBOX_TILES =
    ".status__media a.media__link, .status__media video.media__gifv, " +
    ".media-wall a.media-wall__tile[data-media-url]";

  function bindLightboxes(root) {
    root.querySelectorAll(LIGHTBOX_TILES).forEach((tile) => {
      if (tile.dataset.lightbox) return;
      tile.dataset.lightbox = "1";
      tile.addEventListener("click", (event) => {
        // Leave modified clicks (new tab, etc.) to the browser.
        if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
        const gallery = tile.closest(".status__media, .media-wall");
        const tiles = [
          ...gallery.querySelectorAll(
            "a.media__link, video.media__gifv, a.media-wall__tile[data-media-url]",
          ),
        ];
        event.preventDefault();
        openLightbox(tiles.map(lightboxItemOf), tiles.indexOf(tile), tile);
      });
    });
  }

  // ---- Blurhash thumbnail placeholders --------------------------------
  //
  // Paint the attachment's blurhash behind the lazy-loading thumbnail so the
  // reserved box shows a color impression instead of a blank while it loads.
  const BASE83 =
    "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~";

  function base83decode(str) {
    let value = 0;
    for (const c of str) {
      const digit = BASE83.indexOf(c);
      if (digit < 0) return null;
      value = value * 83 + digit;
    }
    return value;
  }

  const srgbToLinear = (v) => {
    const x = v / 255;
    return x <= 0.04045 ? x / 12.92 : Math.pow((x + 0.055) / 1.055, 2.4);
  };
  const linearToSrgb = (v) => {
    const x = Math.max(0, Math.min(1, v));
    return Math.round(
      x <= 0.0031308 ? x * 12.92 * 255 : (1.055 * Math.pow(x, 1 / 2.4) - 0.055) * 255,
    );
  };
  const signPow = (v, e) => Math.sign(v) * Math.pow(Math.abs(v), e);

  // Standard blurhash decode (https://github.com/woltapp/blurhash) onto a
  // small canvas — the CSS background scales it up, blur comes for free.
  function decodeBlurhash(hash, size) {
    if (!hash || hash.length < 6) return null;
    const sizeFlag = base83decode(hash[0]);
    if (sizeFlag === null) return null;
    const numY = Math.floor(sizeFlag / 9) + 1;
    const numX = (sizeFlag % 9) + 1;
    if (hash.length !== 4 + 2 * numX * numY) return null;
    const maxAc = (base83decode(hash[1]) + 1) / 166;
    const colors = [];
    const dc = base83decode(hash.substring(2, 6));
    colors.push([
      srgbToLinear((dc >> 16) & 255),
      srgbToLinear((dc >> 8) & 255),
      srgbToLinear(dc & 255),
    ]);
    for (let i = 1; i < numX * numY; i++) {
      const ac = base83decode(hash.substring(2 + i * 2 + 2, 4 + i * 2 + 2));
      if (ac === null) return null;
      colors.push([
        signPow((Math.floor(ac / (19 * 19)) - 9) / 9, 2) * maxAc,
        signPow(((Math.floor(ac / 19) % 19) - 9) / 9, 2) * maxAc,
        signPow(((ac % 19) - 9) / 9, 2) * maxAc,
      ]);
    }
    const canvas = document.createElement("canvas");
    canvas.width = size;
    canvas.height = size;
    const ctx = canvas.getContext("2d");
    const image = ctx.createImageData(size, size);
    for (let y = 0; y < size; y++) {
      for (let x = 0; x < size; x++) {
        let r = 0;
        let g = 0;
        let b = 0;
        for (let j = 0; j < numY; j++) {
          for (let i = 0; i < numX; i++) {
            const basis =
              Math.cos((Math.PI * x * i) / size) * Math.cos((Math.PI * y * j) / size);
            const color = colors[i + j * numX];
            r += color[0] * basis;
            g += color[1] * basis;
            b += color[2] * basis;
          }
        }
        const at = 4 * (x + y * size);
        image.data[at] = linearToSrgb(r);
        image.data[at + 1] = linearToSrgb(g);
        image.data[at + 2] = linearToSrgb(b);
        image.data[at + 3] = 255;
      }
    }
    ctx.putImageData(image, 0, 0);
    return canvas;
  }

  function bindBlurhashes(root) {
    root.querySelectorAll("img[data-blurhash]").forEach((img) => {
      if (img.dataset.blurhashDone) return;
      img.dataset.blurhashDone = "1";
      if (img.complete) return; // already there (cache) — nothing to cover
      const canvas = decodeBlurhash(img.dataset.blurhash, 32);
      if (!canvas) return;
      // A blob: URL, not canvas.toDataURL(): the CSP's img-src admits blob:
      // (composer previews already rely on it) but deliberately not data:.
      canvas.toBlob((blob) => {
        if (!blob) return;
        const url = URL.createObjectURL(blob);
        const clear = () => {
          img.style.backgroundImage = "";
          URL.revokeObjectURL(url);
        };
        if (img.complete) {
          // The real thumbnail landed while the blob was encoding.
          URL.revokeObjectURL(url);
          return;
        }
        img.style.backgroundImage = `url(${url})`;
        img.addEventListener("load", clear, { once: true });
        img.addEventListener("error", clear, { once: true });
      });
    });
  }

  // ---- Remote A/V warm-up -----------------------------------------------
  //
  // Large remote video and podcast audio are cached on first play: the proxy
  // holds the request briefly, but a long download can outlive that window
  // and 404 while caching keeps running server-side. The media element then
  // fires an error; poll the proxy and retry once the cached copy has landed.
  function bindMediaWarmup(root) {
    root
      .querySelectorAll('video[src*="/media/proxy/"], audio[src*="/media/proxy/"]')
      .forEach((media) => {
        if (media.dataset.warmupBound) return;
        media.dataset.warmupBound = "1";
        media.addEventListener("error", () => beginMediaWarmup(media));
      });
  }

  async function beginMediaWarmup(media) {
    if (media.dataset.warming) return;
    media.dataset.warming = "1";
    const kind = media instanceof HTMLAudioElement ? "audio" : "video";
    const note = document.createElement("span");
    note.className = "media__preparing";
    note.textContent = `Preparing ${kind}…`;
    media.closest("figure")?.append(note);
    const src = media.getAttribute("src");
    // A long download can take minutes to fetch and remux; each probe is also
    // held briefly by the server, so this paces itself. Give up after ~30
    // minutes of trying.
    for (let attempt = 0; attempt < 120; attempt++) {
      await new Promise((resolve) => setTimeout(resolve, 5000));
      try {
        const res = await fetch(src, { method: "HEAD" });
        if (res.ok) {
          note.remove();
          delete media.dataset.warming;
          media.load();
          media.play().catch(() => {});
          return;
        }
      } catch (_err) {
        // Transient network trouble: keep waiting.
      }
    }
    note.textContent = `${kind[0].toUpperCase()}${kind.slice(1)} is still preparing — try again later.`;
  }

  // ---- HLS video (PeerTube) --------------------------------------------
  //
  // An HLS-native remote video ships `data-hls` = the caching proxy's master
  // playlist. We take over playback so the browser never fetches the whole
  // progressive mp4: hls.js on MSE browsers (quality selector + separated-audio
  // sound), the native player on Safari/iOS, and — only if neither works — the
  // progressive `src` we replace on first play. hls.js is a ~400 KB asset, so
  // it is loaded lazily on the first play, never on a timeline scroll.
  let hlsLoading = null;
  function loadHls(cb) {
    if (window.Hls) return cb(window.Hls);
    if (hlsLoading) return hlsLoading.push(cb);
    hlsLoading = [cb];
    const script = document.createElement("script");
    // The `?v=` buster must track the vendored hls.js version: the asset is
    // served immutable, so a bare URL would pin browsers to a stale build.
    script.src = "/assets/hls.min.js?v=1.6.16";
    const flush = (value) => {
      const queue = hlsLoading || [];
      hlsLoading = null;
      queue.forEach((fn) => fn(value));
    };
    script.onload = () => flush(window.Hls || null);
    script.onerror = () => flush(null);
    document.head.appendChild(script);
  }

  // Called on both MANIFEST_PARSED (video levels) and AUDIO_TRACKS_UPDATED: hls.js
  // populates hls.audioTracks only on the latter, AFTER the former, so the
  // "Audio only" entry can only be added on the second pass. Idempotent — builds
  // the <select> once, then adds the audio option once its track is known.
  function buildQualityMenu(video, hls, Hls) {
    const figure = video.closest("figure") || video.parentNode;
    let select = figure.querySelector(".video-quality");
    const levels = hls.levels || [];
    // PeerTube ships separated audio as an EXT-X-MEDIA alternate track (its own
    // media playlist); hls.js resolves it to an absolute same-origin proxied url
    // we can play video-less.
    const audioUrl = ((hls.audioTracks || [])[0] || {}).url;
    if (!select) {
      if (levels.length < 2 && !audioUrl) return; // nothing to choose yet
      const masterUrl = video.getAttribute("data-hls");
      select = document.createElement("select");
      select.className = "video-quality";
      select.setAttribute("aria-label", "Video quality");
      const addOption = (value, label) => {
        const option = document.createElement("option");
        option.value = value;
        option.textContent = label;
        select.appendChild(option);
      };
      addOption("-1", "Auto");
      levels.forEach((level, index) => {
        addOption(
          String(index),
          level.height
            ? level.height + "p"
            : level.bitrate
              ? Math.round(level.bitrate / 1000) + "k"
              : "Audio",
        );
      });
      select.value = "-1";
      let audioMode = false;
      select.addEventListener("change", () => {
        if (select.value === "audio") {
          // Read the url fresh — it is only known after AUDIO_TRACKS_UPDATED.
          const url = ((hls.audioTracks || [])[0] || {}).url;
          if (!url) return;
          audioMode = true;
          hls.loadSource(url); // the audio media playlist carries no video
          return;
        }
        const level = parseInt(select.value, 10);
        if (audioMode) {
          // Coming back from audio-only: reload the video master, then apply the
          // chosen level once its manifest is parsed.
          audioMode = false;
          hls.once(Hls.Events.MANIFEST_PARSED, () => {
            hls.currentLevel = level;
          });
          hls.loadSource(masterUrl);
        } else {
          hls.currentLevel = level;
        }
      });
      hls.on(Hls.Events.LEVEL_SWITCHED, () => {
        if (!audioMode && hls.autoLevelEnabled) select.value = "-1";
      });
      figure.appendChild(select);
    }
    // The explicit audio-only choice (PeerTube offers it; saves data), added
    // once its track and url are known.
    if (audioUrl && !select.querySelector('option[value="audio"]')) {
      const option = document.createElement("option");
      option.value = "audio";
      option.textContent = "Audio only";
      select.appendChild(option);
    }
  }

  function initHlsVideo(video) {
    const hlsUrl = video.getAttribute("data-hls");
    if (!hlsUrl) return;
    // The progressive proxy is a real `src` in server-rendered HTML so script
    // blockers and CSP failures retain a working player. Once JS is running,
    // preserve it as the fallback. It must remain attached until the viewer
    // presses play: with no source the browser disables its native play button,
    // so the very `play` event that bootstraps hls.js can never happen.
    const progressive =
      video.getAttribute("src") || video.getAttribute("data-src");
    const native = video.canPlayType("application/vnd.apple.mpegurl");
    video.addEventListener(
      "play",
      function onPlay() {
        // The progressive source existed only to keep the native control
        // actionable. Stop its just-started request before selecting HLS, so a
        // click never downloads both the MP4 fallback and the HLS ladder.
        video.pause();
        video.removeAttribute("src");
        video.load();
        // hls.js is the ONLY player that handles PeerTube's multi-rendition
        // ladder + separated-audio alternate track, so it is preferred wherever
        // MediaSource exists (Chrome/Firefox/desktop Safari) — Chrome's *native*
        // HLS silently drops the alternate audio. Only iOS / old Safari (native
        // HLS, no MSE) play directly, skipping the hls.js download.
        if (!window.MediaSource && native) {
          video.src = hlsUrl;
          video.play().catch(() => {});
          return;
        }
        loadHls((Hls) => {
          if (Hls && Hls.isSupported()) {
            // A live stream has no beginning to seek to and a window that
            // moves out from under a stalled player, so it starts at the live
            // edge and is told to chase it back after a buffer hole rather
            // than sitting where it fell behind.
            const live = video.dataset.live === "live";
            const hls = new Hls(
              live
                ? {
                    capLevelToPlayerSize: true,
                    lowLatencyMode: true,
                    liveSyncDurationCount: 3,
                    // The origin deletes segments as the window slides; a
                    // player that keeps retrying a gone segment never
                    // recovers, so let it jump the gap instead.
                    maxBufferHole: 1,
                  }
                : { capLevelToPlayerSize: true },
            );
            hls.loadSource(hlsUrl);
            hls.attachMedia(video);
            hls.on(Hls.Events.MANIFEST_PARSED, () => {
              buildQualityMenu(video, hls, Hls);
              video.play().catch(() => {});
            });
            if (live) {
              // A live playlist can hiccup — a segment rolls out of the
              // window mid-fetch — and that is worth resuming from. A
              // broadcast that has *ended* looks the same at first and never
              // recovers, so the retries are counted and the player gives up
              // rather than hammering an origin that has nothing left to
              // serve. The page then shows the ended state on its next load.
              let recoveries = 0;
              hls.on(Hls.Events.ERROR, (_evt, data) => {
                if (!data || !data.fatal) return;
                if (data.type === Hls.ErrorTypes.NETWORK_ERROR && recoveries < 3) {
                  recoveries += 1;
                  hls.startLoad();
                  return;
                }
                hls.destroy();
              });
            }
            // Separated audio (hls.audioTracks) is only known after
            // MANIFEST_PARSED — refresh so the "Audio only" entry appears.
            hls.on(Hls.Events.AUDIO_TRACKS_UPDATED, () => {
              buildQualityMenu(video, hls, Hls);
            });
          } else if (native) {
            video.src = hlsUrl; // native HLS (Safari/iOS)
            video.play().catch(() => {});
          } else if (progressive) {
            video.src = progressive; // last-resort progressive playback
            video.play().catch(() => {});
          }
        });
      },
      { once: true },
    );
  }

  function bindHlsVideos(root) {
    root.querySelectorAll("video[data-hls]").forEach((video) => {
      if (video.dataset.hlsBound) return;
      video.dataset.hlsBound = "1";
      initHlsVideo(video);
    });
  }

  // ---- Web Push settings ------------------------------------------------
  //
  // The /settings/push page renders inert scaffolding — enable button, alert
  // checkboxes, disable button, all hidden. Here we reveal them when the
  // platform can actually deliver pushes and drive the session-authenticated
  // /api/web/push_subscriptions endpoints. The server-side subscription id
  // and alert map are cached in localStorage (subscriptions are per browser);
  // a lost cache is healed by re-registering — the server replaces the
  // subscription per session token, so the POST is idempotent.
  const PUSH_CACHE = "plamenu:push";

  function pushCache() {
    try {
      return JSON.parse(localStorage.getItem(PUSH_CACHE));
    } catch {
      return null;
    }
  }

  // URL-safe base64 (the VAPID public key) → the BufferSource subscribe wants.
  function pushKeyBytes(base64) {
    const pad = "=".repeat((4 - (base64.length % 4)) % 4);
    const raw = atob((base64 + pad).replace(/-/g, "+").replace(/_/g, "/"));
    return Uint8Array.from(raw, (c) => c.charCodeAt(0));
  }

  async function pushApi(method, path, body, serverError) {
    const res = await fetch(path, {
      method,
      credentials: "same-origin",
      headers: body ? { "Content-Type": "application/json" } : {},
      body: body ? JSON.stringify(body) : undefined,
    });
    if (!res.ok) {
      throw new Error(`${serverError} ${res.status}`);
    }
    return method === "DELETE" ? null : res.json();
  }

  function bindPush(root) {
    const box = root.querySelector("[data-push]");
    if (!box || box.dataset.bound) return;
    box.dataset.bound = "1";
    const nojs = document.querySelector("[data-push-nojs]");
    const supported =
      "serviceWorker" in navigator &&
      "PushManager" in window &&
      "Notification" in window;
    if (!supported) {
      if (nojs) {
        nojs.textContent = box.dataset.pushUnsupported;
      }
      return;
    }
    if (nojs) nojs.hidden = true;
    box.hidden = false;

    const enableBtn = box.querySelector("[data-push-enable]");
    const form = box.querySelector("[data-push-alerts]");
    const disableBtn = box.querySelector("[data-push-disable]");
    const status = box.querySelector("[data-push-status]");
    const message = (name) => box.dataset[name] || "";
    const say = (text) => {
      status.textContent = text;
      status.hidden = !text;
    };
    const boxes = () => [...form.querySelectorAll('input[type="checkbox"]')];
    const alerts = () =>
      Object.fromEntries(boxes().map((b) => [b.name, b.checked]));
    const showEnabled = (saved) => {
      if (saved && saved.alerts) {
        boxes().forEach((b) => {
          if (b.name in saved.alerts) b.checked = !!saved.alerts[b.name];
        });
      }
      enableBtn.hidden = true;
      form.hidden = false;
    };
    const showDisabled = () => {
      enableBtn.hidden = false;
      form.hidden = true;
    };

    // Registers (or re-syncs) this browser's subscription with the server and
    // refreshes the cache. `existing` reuses a live browser subscription
    // instead of minting a new one.
    async function register(existing) {
      await navigator.serviceWorker.register("/sw.js");
      // `subscribe` needs an *active* worker; a first-ever registration is
      // still installing when register() resolves, so wait for activation.
      const reg = await navigator.serviceWorker.ready;
      const sub =
        existing ||
        (await reg.pushManager.subscribe({
          userVisibleOnly: true,
          applicationServerKey: pushKeyBytes(box.dataset.pushKey),
        }));
      const keys = sub.toJSON().keys || {};
      const cached = pushCache();
      const saved = await pushApi("POST", "/api/web/push_subscriptions", {
        subscription: {
          endpoint: sub.endpoint,
          standard: true,
          keys: { p256dh: keys.p256dh, auth: keys.auth },
        },
        data: {
          policy: "all",
          alerts: cached && cached.alerts ? cached.alerts : alerts(),
        },
      }, message("pushServerError"));
      localStorage.setItem(
        PUSH_CACHE,
        JSON.stringify({ id: saved.id, alerts: saved.alerts }),
      );
      return saved;
    }

    // The push-service handshake happens outside our control and can stall
    // (offline, or the browser's push backend unreachable) — bound it so the
    // button never hangs silently.
    const timed = (promise) =>
      Promise.race([
        promise,
        new Promise((_, reject) =>
          setTimeout(
            () =>
              reject(
                new Error(
                  message("pushTimeout"),
                ),
              ),
            20000,
          ),
        ),
      ]);

    enableBtn.addEventListener("click", async () => {
      try {
        if ((await Notification.requestPermission()) !== "granted") {
          say(
            message("pushPermissionBlocked"),
          );
          return;
        }
        say(message("pushContacting"));
        showEnabled(await timed(register(null)));
        say(message("pushEnabled"));
      } catch (err) {
        say(`${message("pushEnableFailed")} ${err.message}`);
      }
    });

    form.addEventListener("change", async () => {
      const cached = pushCache();
      if (!cached) return;
      try {
        const saved = await pushApi(
          "PATCH",
          `/api/web/push_subscriptions/${cached.id}`,
          { data: { alerts: alerts() } },
          message("pushServerError"),
        );
        localStorage.setItem(
          PUSH_CACHE,
          JSON.stringify({ id: saved.id, alerts: saved.alerts }),
        );
        say(message("pushSaved"));
      } catch (err) {
        say(`${message("pushSaveFailed")} ${err.message}`);
      }
    });

    disableBtn.addEventListener("click", async () => {
      say("");
      try {
        const cached = pushCache();
        if (cached) {
          await pushApi(
            "DELETE",
            `/api/web/push_subscriptions/${cached.id}`,
            undefined,
            message("pushServerError"),
          ).catch(() => {});
        }
        const reg = await navigator.serviceWorker.getRegistration("/sw.js");
        const sub = reg && (await reg.pushManager.getSubscription());
        if (sub) await sub.unsubscribe();
        localStorage.removeItem(PUSH_CACHE);
        showDisabled();
        say(message("pushDisabled"));
      } catch (err) {
        say(`${message("pushDisableFailed")} ${err.message}`);
      }
    });

    // Initial state: a live browser subscription means "on"; heal a lost
    // cache by re-registering it with the server.
    (async () => {
      try {
        const reg = await navigator.serviceWorker.getRegistration("/sw.js");
        const sub = reg && (await reg.pushManager.getSubscription());
        if (!sub) {
          showDisabled();
          return;
        }
        const cached = pushCache();
        showEnabled(cached || (await register(sub)));
      } catch {
        showDisabled();
      }
    })();
  }

  // ---- Remote profile history -----------------------------------------
  //
  // The form works through PRG without JavaScript. Here it submits in place,
  // polls only our local durable state a maximum of three times, and replaces
  // the profile region from a read-only local render when something changes.
  // No browser request goes directly to the remote host or reloads the page.
  function bindRemoteHistory(root) {
    root.querySelectorAll("[data-remote-history]").forEach((panel) => {
      if (panel.dataset.bound) return;
      panel.dataset.bound = "1";
      const delays = [1200, 2200, 4000];
      let polls = 0;
      let polling = ["queued", "fetching"].includes(panel.dataset.historyState);
      let timer = null;

      const replaceRegion = async () => {
        const current = panel.closest("[data-remote-history-region]");
        if (!current) return false;
        const res = await fetch(window.location.href, {
          headers: { "X-Remote-History-Fragment": "1" },
          credentials: "same-origin",
        });
        if (!res.ok) throw new Error(res.statusText);
        const doc = new DOMParser().parseFromString(await res.text(), "text/html");
        const incoming = doc.querySelector("[data-remote-history-region]");
        if (!incoming) return false;
        const nextPanel = incoming.querySelector("[data-remote-history]");
        if (nextPanel) nextPanel.dataset.bound = "1";
        const scroll = window.scrollY;
        current.replaceWith(incoming);
        enhance(incoming);
        watchPager(document);
        window.scrollTo(0, scroll);
        if (!nextPanel) {
          polling = false;
          return false;
        }
        panel = nextPanel;
        bindForm();
        return true;
      };

      const stopInBackground = () => {
        polling = false;
        const label = panel.querySelector("[data-remote-history-state-text]");
        if (label && panel.dataset.historyBackground) {
          label.textContent = panel.dataset.historyBackground;
        }
        panel.querySelector("button[disabled]")?.remove();
      };

      const schedulePoll = () => {
        if (!polling) return;
        if (polls >= delays.length) {
          stopInBackground();
          return;
        }
        timer = setTimeout(poll, delays[polls]);
      };

      const poll = async () => {
        if (!polling) return;
        polls += 1;
        try {
          const res = await fetch(panel.dataset.stateUrl, {
            headers: { "X-Requested-With": "fetch" },
            credentials: "same-origin",
          });
          const data = res.ok ? await res.json() : null;
          if (data) {
            const busy = ["queued", "fetching"].includes(data.state);
            const changed =
              data.state !== panel.dataset.historyState ||
              String(data.available_statuses) !== panel.dataset.historyAvailable;
            panel.dataset.historyState = data.state;
            panel.dataset.historyAvailable = String(data.available_statuses);
            if (changed || !busy || !data.enabled) {
              await replaceRegion();
            }
            polling = busy && data.enabled;
          }
        } catch (_err) {
          // A transient local read failure consumes one of the bounded polls.
        }
        schedulePoll();
      };

      function bindForm() {
        const form = panel.querySelector("[data-remote-history-form]");
        if (!form || form.dataset.bound) return;
        form.dataset.bound = "1";
        const button = form.querySelector("button");
        form.addEventListener("submit", async (event) => {
          event.preventDefault();
          if (button) button.disabled = true;
          try {
            const res = await fetch(form.action, {
              method: "POST",
              body: new URLSearchParams(new FormData(form)),
              headers: {
                "Content-Type": "application/x-www-form-urlencoded",
                "X-Requested-With": "fetch",
              },
              credentials: "same-origin",
            });
            if (!res.ok) throw new Error(res.statusText);
            const data = await res.json();
            polling = ["queued", "fetching"].includes(data.state);
            polls = 0;
            if (timer) clearTimeout(timer);
            await replaceRegion();
            schedulePoll();
          } catch (_err) {
            if (button) button.disabled = false;
          }
        });
      }

      bindForm();
      schedulePoll();
    });
  }

  // ---- Installed-app shell --------------------------------------------
  // Register on every page, not only after push notifications are enabled:
  // installation metadata can now pair with a root-scoped worker that keeps
  // the branded offline fallback and its minimal static shell available. A
  // failed registration never changes the server-rendered experience.
  function registerAppWorker() {
    if (!("serviceWorker" in navigator)) return;
    navigator.serviceWorker
      .register("/sw.js", { scope: "/", updateViaCache: "none" })
      .catch(() => {});
  }

  // ---- Live notification indicator and cue -----------------------------
  // The server's Mastodon-compatible streaming endpoint already emits
  // `notification` events. The first-party session cookie authenticates this
  // same-origin socket without exposing its HttpOnly token to JavaScript.
  let notificationAudio = null;

  function primeNotificationAudio() {
    if (document.body?.dataset.notificationSound !== "true") return;
    const AudioContext = window.AudioContext || window.webkitAudioContext;
    if (!AudioContext) return;
    try {
      notificationAudio ||= new AudioContext();
      if (notificationAudio.state === "suspended") notificationAudio.resume().catch(() => {});
    } catch (_err) {
      notificationAudio = null;
    }
  }

  function playNotificationCue() {
    if (
      document.body.dataset.notificationSound !== "true" ||
      document.visibilityState !== "visible"
    ) return;
    const volume = Math.min(
      1,
      Math.max(0, Number(document.body.dataset.notificationVolume || 0) / 100),
    );
    if (!volume) return;
    primeNotificationAudio();
    if (!notificationAudio || notificationAudio.state !== "running") return;
    const now = notificationAudio.currentTime;
    const gain = notificationAudio.createGain();
    const tone = notificationAudio.createOscillator();
    tone.type = "sine";
    tone.frequency.setValueAtTime(740, now);
    tone.frequency.exponentialRampToValueAtTime(980, now + 0.08);
    gain.gain.setValueAtTime(0.0001, now);
    gain.gain.exponentialRampToValueAtTime(Math.max(0.0001, volume * 0.12), now + 0.015);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + 0.12);
    tone.connect(gain);
    gain.connect(notificationAudio.destination);
    tone.start(now);
    tone.stop(now + 0.13);
  }

  function showLiveNotification() {
    document.querySelectorAll("[data-notification-nav] .bell").forEach((bell) => {
      if (!bell.querySelector(".bell__dot")) {
        const dot = document.createElement("span");
        dot.className = "bell__dot";
        bell.append(dot);
      }
    });
    playNotificationCue();
  }

  function bindLiveNotifications() {
    const body = document.body;
    if (
      !body ||
      body.dataset.liveNotifications !== "true" ||
      body.dataset.liveNotificationsBound
    ) return;
    body.dataset.liveNotificationsBound = "1";
    // Browsers only allow audio after a user gesture. Prime the tiny synth on
    // the first interaction so a later streaming event can play immediately.
    document.addEventListener("pointerdown", primeNotificationAudio, { once: true });
    document.addEventListener("keydown", primeNotificationAudio, { once: true });

    let socket = null;
    let suspended = false;
    let retries = 0;
    let reconnectTimer = null;
    const connect = () => {
      if (suspended || !navigator.onLine || socket) return;
      const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
      let nextSocket;
      try {
        nextSocket = new WebSocket(
          `${scheme}//${window.location.host}/api/v1/streaming?stream=user:notification`,
        );
      } catch (_err) {
        scheduleReconnect();
        return;
      }
      socket = nextSocket;
      nextSocket.addEventListener("open", () => { retries = 0; });
      nextSocket.addEventListener("message", (event) => {
        try {
          const message = JSON.parse(event.data);
          if (message.event === "notification") showLiveNotification();
        } catch (_err) {
          // Ignore protocol noise or an extension event this client does not
          // understand; the socket remains usable.
        }
      });
      nextSocket.addEventListener("close", () => {
        // A pagehide deliberately detaches `socket` before closing it. Ignore
        // that close event (and any stale connection's event) so a frozen
        // bfcache entry never schedules background work.
        if (socket !== nextSocket) return;
        socket = null;
        scheduleReconnect();
      });
      nextSocket.addEventListener("error", () => nextSocket.close());
    };
    const scheduleReconnect = () => {
      if (suspended || reconnectTimer) return;
      const delay = Math.min(30000, 1000 * 2 ** Math.min(retries, 5));
      retries += 1;
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        connect();
      }, delay);
    };
    const suspend = () => {
      suspended = true;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      reconnectTimer = null;
      const active = socket;
      socket = null;
      // Open WebSockets can make a page ineligible for the browser's
      // back/forward cache. Close before navigation; the identity guard in the
      // close listener keeps this intentional shutdown from reconnecting.
      if (active) active.close();
    };
    const resume = () => {
      suspended = false;
      connect();
    };
    window.addEventListener("online", () => {
      if (reconnectTimer) clearTimeout(reconnectTimer);
      reconnectTimer = null;
      connect();
    });
    window.addEventListener("pagehide", suspend);
    window.addEventListener("pageshow", resume);
    connect();
  }

  function bindFileLimits(root) {
    root.querySelectorAll('input[type="file"][data-max-bytes]').forEach((input) => {
      if (input.dataset.sizeBound) return;
      input.dataset.sizeBound = "1";
      input.addEventListener("change", () => {
        const limit = Number(input.dataset.maxBytes);
        const oversized = Array.from(input.files).some((file) => file.size > limit);
        input.setCustomValidity(oversized ? `Choose a file no larger than ${limit / 1048576} MiB.` : "");
        if (oversized) input.reportValidity();
      });
    });
  }

  // ---- Wiring -----------------------------------------------------------
  function enhance(root) {
    bindFileLimits(root);
    bindActionForms(root);
    bindPollForms(root);
    bindRsvpForms(root);
    bindTranslateForms(root);
    bindDisclosures(root, "details[data-status-menu]", { pop: ".status__menu-pop" });
    bindDisclosures(root, "details[data-nav-select]", { pop: ".nav-select__pop" });
    bindDisclosures(root, "details[data-drawer]", { dismissed: scrimTapped });
    bindStandaloneExternalLinks(root);
    bindCopyLinks(root);
    bindConfirms(root);
    bindModerationForms(root);
    bindReportForms(root);
    bindAnnouncementDismiss(root);
    bindReactionPickers(root);
    bindReactionForms(root);
    bindReactionTooltips(root);
    bindLightboxes(root);
    bindBlurhashes(root);
    bindHlsVideos(root);
    bindMediaWarmup(root);
    bindCompose(root);
    bindComposeCombo(root);
    bindLanguageControls(root);
    bindSettingsDeps(root);
    bindWebauthn(root);
    bindPush(root);
    bindRemoteHistory(root);
    bindLiveNotifications();
    refreshTimes(root);
  }

  document.addEventListener("DOMContentLoaded", () => {
    registerAppWorker();
    enhance(document);
    watchPager(document);
    setInterval(() => refreshTimes(document), 60000);
  });
})();
