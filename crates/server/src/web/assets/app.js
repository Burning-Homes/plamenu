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
    const swapLanguage = (el) => {
      if (!el || !data.language) return;
      const original = el.getAttribute("lang");
      restores.push(() => {
        if (original == null) el.removeAttribute("lang");
        else el.setAttribute("lang", original);
      });
      el.setAttribute("lang", data.language);
    };
    const swapHtml = (el, html) => {
      if (!el || !html) return;
      const original = el.innerHTML;
      restores.push(() => { el.innerHTML = original; });
      el.innerHTML = html;
      swapLanguage(el);
    };
    const swapText = (el, text) => {
      if (!el || !text) return;
      const original = el.innerHTML;
      restores.push(() => { el.innerHTML = original; });
      el.textContent = text;
      swapLanguage(el);
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
  // the POST behind a confirmation prompt. Without JS the rendered action
  // leads to the server's equivalent review step.
  function bindConfirms(root) {
    root.querySelectorAll("form[data-confirm]").forEach((form) => {
      if (form.dataset.confirmBound) return;
      form.dataset.confirmBound = "1";
      // The server-rendered action is the no-JS review step. Once this
      // enhancement is active, restore the real endpoint and use the compact
      // native prompt; all later action-form code therefore sees the same
      // endpoint it did before progressive enhancement.
      if (form.dataset.confirmAction) form.action = form.dataset.confirmAction;
      form.addEventListener("submit", (event) => {
        // Read at submit time: the moderation toggle below removes the
        // prompt while a form is flipped to its (harmless) undo direction.
        if (form.dataset.confirm && !window.confirm(form.dataset.confirm)) {
          event.preventDefault();
        }
      });
    });
  }

  // The pending-application table exposes two deliberately distinct scopes:
  // visible row checkboxes and an all-matching snapshot. Keep the convenience
  // toggles mutually exclusive; the server remains authoritative without JS.
  function bindApplicationBulkSelection(root) {
    root.querySelectorAll("form.admin-bulk").forEach((form) => {
      if (form.dataset.bulkBound) return;
      form.dataset.bulkBound = "1";
      const page = form.querySelector("[data-bulk-page]");
      const matching = form.querySelector("[data-bulk-matching]");
      if (page) {
        page.addEventListener("change", () => {
          form.querySelectorAll("[data-bulk-row]").forEach((row) => {
            row.checked = page.checked;
          });
          if (page.checked && matching) matching.checked = false;
        });
      }
      if (matching) {
        matching.addEventListener("change", () => {
          if (!matching.checked) return;
          if (page) page.checked = false;
          form.querySelectorAll("[data-bulk-row]").forEach((row) => {
            row.checked = false;
          });
        });
      }
    });
    root.querySelectorAll("form[data-bulk-continue]").forEach((form) => {
      if (form.dataset.bulkContinueBound) return;
      form.dataset.bulkContinueBound = "1";
      queueMicrotask(() => form.requestSubmit());
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
  const MENTION_RE = /(^|[^/\p{Alphabetic}\p{N}\p{M}_@])@([\p{Alphabetic}\p{N}\p{M}_.-]+)@([\p{Alphabetic}\p{N}\p{M}.\-\u3002\uff0e\uff61]+)/giu;
  const MENTION_WORD_RE = /^[\p{Alphabetic}\p{N}\p{M}_@]/u;
  const usernameEncoder = new TextEncoder();
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
      .replace(MENTION_RE, (whole, prefix, username, host, offset, source) => {
        const domain = host.replace(/\.+$/, "");
        if (!/[.\u3002\uff0e\uff61]/u.test(domain)) return whole;
        let asciiDomain;
        try {
          asciiDomain = new URL(`https://${domain}/`).hostname;
        } catch (_) {
          return whole;
        }
        // Keep invalid handles whole, just as the server does: neither a
        // username prefix nor a partial host is a mention of another account.
        if (/^[.\-\p{M}]|[.\-]$/u.test(username)
          || usernameEncoder.encode(username).length > 2048
          || !asciiDomain.includes(".") || asciiDomain.length > 253 || !/^[a-z0-9.-]+$/i.test(asciiDomain)
          || asciiDomain.split(".").some((label) => !label || label.length > 63)
          || MENTION_WORD_RE.test(source.slice(offset + whole.length))) return whole;
        return `${prefix}@${username}${host.slice(domain.length)}`;
      });
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
    pop.style.removeProperty("--popup-translate-x");
    const rect = trigger.getBoundingClientRect();
    const height = pop.offsetHeight;
    const viewportWidth = document.documentElement.clientWidth;
    const tabbar = document.querySelector(".tabbar");
    const usableBottom =
      tabbar && getComputedStyle(tabbar).display !== "none"
        ? Math.min(window.innerHeight, tabbar.getBoundingClientRect().top)
        : window.innerHeight;
    const spaceBelow = usableBottom - rect.bottom;
    const spaceAbove = rect.top;
    const below = spaceBelow >= height + 8 || spaceBelow >= spaceAbove;
    pop.classList.add(below ? "is-below" : "is-above");
    if (rect.left + pop.offsetWidth > viewportWidth - 8) {
      pop.classList.add("is-end");
    }
    // End alignment fixes a right spill, but can push a wide popup past the
    // left edge. Clamp the final physical box in either direction.
    const popRect = pop.getBoundingClientRect();
    let translateX = 0;
    if (popRect.left < 8) translateX = 8 - popRect.left;
    if (popRect.right + translateX > viewportWidth - 8) {
      translateX += viewportWidth - 8 - (popRect.right + translateX);
    }
    if (translateX) {
      pop.style.setProperty("--popup-translate-x", `${translateX}px`);
    }
  }

  function openPopup(trigger, pop, onOpen) {
    if (closeOpenPopup) closeOpenPopup();
    pop.hidden = false;
    trigger.setAttribute("aria-expanded", "true");
    const combobox = pop.querySelector('[role="combobox"]');
    if (combobox) combobox.setAttribute("aria-expanded", "true");
    function close() {
      pop.hidden = true;
      trigger.setAttribute("aria-expanded", "false");
      if (combobox) {
        combobox.setAttribute("aria-expanded", "false");
        combobox.removeAttribute("aria-activedescendant");
      }
      pop.classList.remove("is-above", "is-below", "is-end");
      pop.style.removeProperty("--popup-translate-x");
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
      } else if (event.key === "Tab") {
        // Let the browser move focus first, then dismiss once it has left both
        // the trigger and popup. Do not pull focus back on an ordinary Tab.
        setTimeout(() => {
          if (!pop.contains(document.activeElement) && document.activeElement !== trigger) {
            close();
          }
        }, 0);
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
      const focusSelected = () => {
        const active = options.find((o) => o.dataset.value === select.value);
        (active || options[0])?.focus();
      };

      togglePopup(trigger, pop, focusSelected);
      trigger.addEventListener("keydown", (event) => {
        if (!["ArrowDown", "ArrowUp"].includes(event.key)) return;
        event.preventDefault();
        if (trigger.getAttribute("aria-expanded") !== "true") {
          openPopup(trigger, pop, focusSelected);
        }
      });

      let typeahead = "";
      let typeaheadTimer = null;
      const focusAt = (index) => {
        options[(index + options.length) % options.length]?.focus();
      };

      options.forEach((option, index) => {
        option.addEventListener("keydown", (event) => {
          if (event.key === "ArrowDown") {
            event.preventDefault();
            focusAt(index + 1);
          } else if (event.key === "ArrowUp") {
            event.preventDefault();
            focusAt(index - 1);
          } else if (event.key === "Home") {
            event.preventDefault();
            focusAt(0);
          } else if (event.key === "End") {
            event.preventDefault();
            focusAt(options.length - 1);
          } else if (
            event.key.length === 1 &&
            !event.altKey && !event.ctrlKey && !event.metaKey
          ) {
            typeahead += event.key.toLocaleLowerCase();
            clearTimeout(typeaheadTimer);
            typeaheadTimer = setTimeout(() => { typeahead = ""; }, 700);
            const match = options.find((candidate) =>
              candidate.querySelector(".compose__menu-option-label")?.textContent
                .trim().toLocaleLowerCase().startsWith(typeahead)
            );
            if (match) {
              event.preventDefault();
              match.focus();
            }
          }
        });
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

      if (select.required) {
        const validation = combo.querySelector("[data-combo-validation]");
        select.addEventListener("invalid", (event) => {
          event.preventDefault();
          if (validation) {
            validation.textContent = combo.dataset.requiredLabel;
            validation.hidden = false;
          }
          trigger.setAttribute("aria-invalid", "true");
          trigger.focus();
        });
        select.addEventListener("change", () => {
          trigger.removeAttribute("aria-invalid");
          if (validation) validation.hidden = true;
        });
      }

      const entries = [...select.options].map((option) => ({
        value: option.value,
        label: option.textContent,
      }));
      let activeIndex = -1;

      const options = () => [
        ...list.querySelectorAll('.compose__combo-option[role="option"]'),
      ];
      const setActive = (index) => {
        const current = options();
        current.forEach((option) => option.classList.remove("is-active"));
        if (!current.length) {
          activeIndex = -1;
          if (search) search.removeAttribute("aria-activedescendant");
          return;
        }
        activeIndex = Math.max(0, Math.min(index, current.length - 1));
        const active = current[activeIndex];
        active.classList.add("is-active");
        if (search) search.setAttribute("aria-activedescendant", active.id);
        active.scrollIntoView({ block: "nearest" });
      };

      const choose = (option) => {
        select.value = option.dataset.value;
        select.dispatchEvent(new Event("change", { bubbles: true }));
        if (labelSlot) labelSlot.textContent = option.textContent;
        if (closeOpenPopup) closeOpenPopup();
        trigger.focus();
      };

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
          empty.setAttribute("role", "option");
          empty.setAttribute("aria-disabled", "true");
          empty.textContent = combo.dataset.emptyLabel || "No languages found";
          list.append(empty);
          setActive(-1);
          return;
        }
        kept.forEach((entry, index) => {
          const option = document.createElement("li");
          option.id = `${list.id}-option-${index}`;
          option.className = "compose__combo-option";
          option.setAttribute("role", "option");
          option.dataset.value = entry.value;
          option.textContent = entry.label;
          option.setAttribute("aria-selected", String(entry.value === select.value));
          option.addEventListener("click", () => choose(option));
          list.append(option);
        });
        const selected = kept.findIndex((entry) => entry.value === select.value);
        setActive(selected >= 0 ? selected : 0);
      };

      togglePopup(trigger, pop, () => {
        render(search ? search.value : "");
        if (search) search.focus();
      });

      trigger.addEventListener("keydown", (event) => {
        if (!["ArrowDown", "ArrowUp"].includes(event.key)) return;
        event.preventDefault();
        if (trigger.getAttribute("aria-expanded") !== "true") {
          openPopup(trigger, pop, () => {
            render(search ? search.value : "");
            if (search) search.focus();
          });
        }
      });

      if (search) {
        search.addEventListener("input", () => render(search.value));
        search.addEventListener("keydown", (event) => {
          const current = options();
          if (!current.length) return;
          if (event.key === "ArrowDown") {
            event.preventDefault();
            setActive(activeIndex < current.length - 1 ? activeIndex + 1 : 0);
          } else if (event.key === "ArrowUp") {
            event.preventDefault();
            setActive(activeIndex > 0 ? activeIndex - 1 : current.length - 1);
          } else if (event.key === "Home") {
            event.preventDefault();
            setActive(0);
          } else if (event.key === "End") {
            event.preventDefault();
            setActive(current.length - 1);
          } else if (event.key === "Enter" && activeIndex >= 0) {
            event.preventDefault();
            choose(current[activeIndex]);
          }
        });
      }
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
    else if (section) { section.hidden = false; section.classList.add("is-open"); }
  }

  let composeMediaFieldSequence = 0;

  function bindComposeMedia(form) {
    const section = form.querySelector('[data-compose-section="media"]');
    if (!section) return;
    const body = section.querySelector(".compose__media-body");
    const staticSlots = section.querySelector("[data-media-static]");
    if (!body || !staticSlots) return;
    const parsedMax = Number.parseInt(section.dataset.mediaMax, 10);
    const max = Number.isFinite(parsedMax) ? parsedMax : 4;

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
    picker.setAttribute("aria-label", form.dataset.i18nMediaAdd || "Add images, video or audio");
    picker.tabIndex = -1;
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
    dropzoneLabel.className = "compose__dropzone-label";
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

    const kindPicker = form.querySelector("[data-compose-kind]");
    const isAudioPost = () => kindPicker?.value === "audio";
    const mediaError = document.createElement("p");
    mediaError.className = "compose__error"; mediaError.setAttribute("role", "alert"); mediaError.hidden = true;
    body.append(mediaError);
    const audioFileError = () => { mediaError.textContent = form.dataset.i18nAudioFile || "Choose one audio file for an Audio post."; mediaError.hidden = false; };
    const existingCount = () => form.querySelectorAll('[name="reuse_media[]"]:checked').length;
    function refreshAddState() {
      form.dataset.composeCurrent = kindPicker?.value || 'note';
      const selected = items.length + existingCount();
      const sensitive = form.querySelector('[data-compose-sensitive]');
      if (sensitive) sensitive.hidden = !selected && !form.querySelector('[name="media_keep[]"]');
      form.querySelectorAll('[name="reuse_media[]"]').forEach(input => {
        let audio = false;
        try { audio = JSON.parse(input.dataset.existingMedia).type === 'audio'; } catch { /* Unusable metadata stays unavailable to audio posts. */ }
        input.disabled = !input.checked && (selected >= (isAudioPost() ? Math.min(1,max) : max) || (isAudioPost() && !audio));
      });
      picker.accept = isAudioPost() ? "audio/*" : MEDIA_ACCEPT;
      picker.multiple = !isAudioPost();
      picker.setAttribute("aria-label", isAudioPost() ? (form.dataset.i18nAudioAdd || "Choose an audio file") : (form.dataset.i18nMediaAdd || "Add images, video or audio"));
      const full = selected >= (isAudioPost() ? Math.min(1,max) : max);
      mediaError.hidden = true;
      if (isAudioPost() && (items.length > 1 || items.some(item => !item.file.type.startsWith("audio/")))) audioFileError();
      dropzone.hidden = full;
      picker.disabled = full;
    }

    function removeItem(item) {
      const idx = items.indexOf(item);
      if (idx === -1) return;
      items.splice(idx, 1);
      item.li.querySelector("audio, video")?.pause();
      if (item.url) URL.revokeObjectURL(item.url);
      if (item.inlineMarker) {
        const field = form.querySelector("textarea[name=status]");
        if (field) field.value = field.value.split(item.inlineMarker).join("");
      }
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
      } else if (file.type.startsWith("audio/")) {
        url = URL.createObjectURL(file);
        li.dataset.audioPreviewUrl = url;
        li.dataset.audioPreviewName = file.name;
        preview.innerHTML = svgIcon(ICON_AUDIO);
      } else {
        preview.innerHTML = svgIcon(
          file.type.startsWith("video/") ? ICON_FILM : ICON_AUDIO
        );
      }

      const altName = document.createElement("label");
      altName.className = "compose__attachment-label";
      altName.textContent = file.type.startsWith("audio/") ? (form.dataset.i18nAudioDescription || "Describe this audio") : form.dataset.i18nAltDescription || "Describe this attachment";
      const alt = document.createElement("textarea");
      const fieldSequence = ++composeMediaFieldSequence;
      alt.id = `compose-media-alt-${fieldSequence}`;
      altName.htmlFor = alt.id;
      alt.name = "media_alt[]";
      alt.rows = 2;
      alt.maxLength = 1500;
      alt.className = "compose__alt";
      const altFileName = document.createElement("span");
      altFileName.className = "visually-hidden";
      altFileName.textContent = ` — ${file.name}`;
      altName.append(altFileName);

      const decorativeLabel = document.createElement("label");
      decorativeLabel.className = "compose__inline compose__media-extra";
      const decorativeDefault = document.createElement("input");
      decorativeDefault.type = "hidden";
      decorativeDefault.name = "media_decorative[]";
      decorativeDefault.value = "false";
      const decorative = document.createElement("input");
      decorative.type = "checkbox";
      decorative.name = "media_decorative[]";
      decorative.value = "true";
      const decorativeText = document.createElement("span");
      decorativeText.textContent =
        form.dataset.i18nDecorative || "This image doesn’t add information";
      decorativeLabel.append(decorativeDefault, decorative, decorativeText);
      decorative.addEventListener("change", () => {
        if (decorative.checked) alt.value = "";
        alt.disabled = decorative.checked;
      });

      const transcriptLabel = document.createElement("label");
      transcriptLabel.className = "compose__field compose__media-extra";
      const transcriptName = document.createElement("span");
      transcriptName.textContent = form.dataset.i18nTranscript || "Transcript";
      const transcript = document.createElement("textarea");
      transcript.name = "media_transcript[]";
      transcript.rows = 3;
      transcript.maxLength = 50000;
      transcript.placeholder =
        form.dataset.i18nTranscriptDescription ||
        "For video, include important visual information";
      transcriptLabel.append(transcriptName, transcript);

      const captionsLabel = document.createElement("label");
      captionsLabel.className = "compose__field compose__media-extra";
      const captionsName = document.createElement("span");
      captionsName.textContent = form.dataset.i18nCaptions || "WebVTT captions";
      const captions = document.createElement("input");
      captions.type = "file";
      captions.name = "media_captions[]";
      captions.accept = ".vtt,text/vtt";
      captionsLabel.append(captionsName, captions);

      const visualAudioLabel = document.createElement("label");
      visualAudioLabel.className = "compose__field compose__media-extra";
      const visualAudioName = document.createElement("span");
      visualAudioName.textContent =
        form.dataset.i18nVisualAudio || "Access to important visuals";
      const visualAudio = document.createElement("select");
      visualAudio.name = "media_visual_audio[]";
      for (const [value, label] of [
        ["", form.dataset.i18nVisualAudioNone || "Not specified"],
        ["audio_description", form.dataset.i18nAudioDescribed || "Audio description included"],
        ["soundtrack", form.dataset.i18nVisualsInAudio || "Soundtrack already explains the visuals"],
      ]) {
        const option = document.createElement("option");
        option.value = value;
        option.textContent = label;
        visualAudio.appendChild(option);
      }
      visualAudioLabel.append(visualAudioName, visualAudio);

      if (file.type.startsWith("image/")) {
        transcriptLabel.hidden = true;
        transcript.disabled = true;
        captionsLabel.hidden = true;
        captions.disabled = true;
        visualAudioLabel.hidden = true;
        visualAudio.disabled = true;
      } else if (file.type.startsWith("audio/")) {
        decorativeLabel.hidden = true;
        decorativeDefault.disabled = true;
        decorative.disabled = true;
        captionsLabel.hidden = true;
        captions.disabled = true;
        visualAudioLabel.hidden = true;
        visualAudio.disabled = true;
      } else {
        decorativeLabel.hidden = true;
        decorativeDefault.disabled = true;
        decorative.disabled = true;
      }

      // The File rides on a real file input so the plain form submit carries it;
      // it follows its alt box in document order, matching the backend pairing.
      const fileInput = document.createElement("input");
      fileInput.type = "file";
      fileInput.name = "media[]";
      // This input only carries already-selected bytes in the form submission.
      fileInput.hidden = true;
      fileInput.className = "visually-hidden";
      const dt = new DataTransfer();
      dt.items.add(file);
      fileInput.files = dt.files;

      let inlineControl = null;
      let inlineMarker = null;
      if (file.type.startsWith("image/")) {
        const token = `upload-${fieldSequence}`;
        inlineMarker = `[[upload:${token}]]`;
        inlineControl = document.createElement("fieldset");
        inlineControl.className = "compose__inline-placement";
        inlineControl.dataset.composeArticleOnly = "";
        const tokenInput = document.createElement("input");
        tokenInput.type = "hidden";
        tokenInput.name = "media_inline_token[]";
        tokenInput.value = token;
        const insert = document.createElement("button");
        insert.type = "button";
        insert.className = "compose__inline-insert";
        insert.textContent = form.dataset.i18nInsertInline || "Insert in article";
        insert.addEventListener("click", () => {
          const field = form.querySelector("textarea[name=status]");
          if (!field) return;
          if (!field.value.includes(inlineMarker)) {
            const start = field.selectionStart ?? field.value.length;
            const end = field.selectionEnd ?? start;
            field.value =
              field.value.slice(0, start) + inlineMarker + field.value.slice(end);
            const caret = start + inlineMarker.length;
            field.setSelectionRange(caret, caret);
            field.dispatchEvent(new Event("input", { bubbles: true }));
          }
          insert.textContent = form.dataset.i18nInlineInserted || "Inserted in article";
          field.focus();
        });
        inlineControl.append(tokenInput, insert);
        const syncInline = () => {
          const article = form.querySelector("[data-compose-kind]")?.value === "article";
          inlineControl.hidden = !article;
          inlineControl.disabled = !article;
        };
        form.querySelector("[data-compose-kind]")?.addEventListener("change", syncInline);
        syncInline();
      }

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
      const details = document.createElement('details'); details.className='compose__attachment-details';
      const summary = document.createElement('summary'); summary.textContent=form.dataset.i18nMediaDetails || 'Description and accessibility';
      details.append(summary, altName);
      details.append(
        alt,
        decorativeLabel,
        transcriptLabel,
        captionsLabel,
        visualAudioLabel,
        ...(inlineControl ? [inlineControl] : []),
        fileInput
      );

      bodyEl.append(details);
      const filename = document.createElement('span'); filename.className='compose__filename';filename.textContent=file.name;
      if (/^(audio|video)\//.test(file.type)) {
        const audioPreview = document.createElement(file.type.startsWith("video/") ? "video" : "audio");
        audioPreview.controls = true; audioPreview.playsInline = true; audioPreview.preload = "metadata"; audioPreview.src = url;
        audioPreview.setAttribute("aria-label", file.name);
        bodyEl.prepend(audioPreview);
      }
      li.append(preview, filename, remove, bodyEl);

      const item = { file, li, url, inlineMarker };
      remove.addEventListener("click", () => removeItem(item));
      return item;
    }

    kindPicker?.addEventListener("change", refreshAddState);
    form.querySelectorAll('[name="reuse_media[]"]').forEach(input => input.addEventListener("change",refreshAddState));
    function addFiles(fileList) {
      if (isAudioPost() && (fileList.length !== 1 || !fileList[0].type.startsWith("audio/"))) { audioFileError(); return; }
      const incoming = [...fileList].filter((f) =>
        /^(image|video|audio)\//.test(f.type)
      );
      if (!incoming.length) return;
      openSection(form, "media");
      for (const file of incoming) {
        if (items.length + existingCount() >= (isAudioPost() ? Math.min(1,max) : max)) break;
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
      const displayAcct = item.display_acct || item.acct;
      return option(`@${displayAcct}`, (button) => {
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
        handle.textContent = `@${displayAcct}`;
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
    const source = form.querySelector("textarea[name=status]")?.value || "";
    return [...form.querySelectorAll(".compose__attachment")].map((card) => {
      const img = card.querySelector(".compose__thumb img");
      const icon = card.querySelector(".compose__thumb");
      const token = card.querySelector('input[name="media_inline_token[]"]')?.value;
      const marker = token ? `[[upload:${token}]]` : null;
      return {
        src: img ? img.src : null,
        audioSrc: card.dataset.audioPreviewUrl || null,
        videoSrc: card.querySelector("video")?.src || null,
        audioName: card.dataset.audioPreviewName || "",
        icon: img ? null : icon ? icon.innerHTML : "",
        alt: card.querySelector(".compose__alt")?.value || "",
        marker,
        inline: marker ? source.includes(marker) : false,
      };
    }).concat([...form.querySelectorAll('input[name="reuse_media[]"]:checked')].map(input => {
      try {
        const media=JSON.parse(input.dataset.existingMedia);
        return {src:media.type==='image'?media.url:null, audioSrc:media.type==='audio'?media.url:null,
          videoSrc:media.type==='video'?media.url:null,alt:media.description||'',icon:'',inline:false};
      } catch {return {icon:'',inline:false};}
    }));
  }

  function previewAltBadge(alt) {
    if (!alt) return null;
    const control = document.createElement("span");
    control.className = "media__alt";
    const button = document.createElement("button");
    button.type = "button";
    button.className = "media__badge";
    button.textContent = "ALT";
    button.setAttribute("aria-label", `ALT: ${alt}`);
    const text = document.createElement("span");
    text.className = "media__alt-text";
    text.setAttribute("role", "tooltip");
    text.textContent = alt;
    control.append(button, text);
    return control;
  }

  // Composites the composer's local thumbnails into the previewed card. Media
  // renders from the same object URLs the composer already holds, so nothing is
  // uploaded on preview (avoids churn and, for video, needless re-encoding) —
  // only the text is server-rendered, which is the drift the preview guards.
  function injectPreviewMedia(pane, media) {
    if (!media.length) return;
    const card = pane.querySelector(".status");
    if (!card) return;
    const content = card.querySelector(".status__content");
    for (const item of content ? media.filter((entry) => entry.inline && entry.src) : []) {
      const walker = document.createTreeWalker(content, NodeFilter.SHOW_TEXT);
      let node;
      while ((node = walker.nextNode())) {
        const offset = node.nodeValue.indexOf(item.marker);
        if (offset < 0) continue;
        const image = document.createElement("img");
        image.className = "status__inline-image";
        image.src = item.src;
        image.alt = item.alt;
        const after = node.splitText(offset);
        after.nodeValue = after.nodeValue.slice(item.marker.length);
        const frame = document.createElement("span");
        frame.className = "status__inline-media";
        frame.appendChild(image);
        const alt = previewAltBadge(item.alt);
        if (alt) frame.appendChild(alt);
        after.before(frame);
        break;
      }
    }
    const attachments = media.filter((item) => !item.inline);
    if (!attachments.length || card.querySelector(".status__media")) return;
    const gallery = document.createElement("div");
    gallery.className = "status__media";
    gallery.dataset.count = String(Math.min(attachments.length, 4));
    for (const item of attachments) {
      const fig = document.createElement("figure");
      fig.className = "media";
      if (item.audioSrc || item.videoSrc) {
        fig.classList.add(item.videoSrc ? 'media--video' : 'media--audio');
        const audio = document.createElement(item.videoSrc ? 'video' : 'audio');
        audio.playsInline = true;
        audio.src = item.videoSrc || item.audioSrc;
        audio.controls = true;
        audio.preload = "metadata";
        audio.setAttribute("aria-label", item.alt || item.audioName);
        fig.appendChild(audio);
      } else if (item.src) {
        const img = document.createElement("img");
        img.src = item.src;
        img.alt = item.alt;
        img.loading = "lazy";
        fig.appendChild(img);
      } else {
        fig.innerHTML = item.icon;
      }
      const alt = previewAltBadge(item.alt);
      if (alt) {
        const badges = document.createElement("div");
        badges.className = "media__badges";
        badges.appendChild(alt);
        fig.appendChild(badges);
      }
      gallery.appendChild(fig);
    }
    const music = content?.parentElement?.querySelector(":scope > .status__music");
    if (music) music.after(gallery);
    else if (content) content.after(gallery);
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
      const workflow = ['video','live'].includes(value);
      const editor = form.querySelector('[data-compose-editor]');
      const route = form.querySelector('[data-compose-workflow]');
      if (editor) editor.hidden = workflow;
      if (route) {
        route.hidden = !workflow;
        if (workflow) {
          route.querySelector('[data-compose-workflow-hint]').textContent = route.querySelector('[data-compose-workflow-hint]').dataset[value];
          route.querySelector('a').href = value === 'live' ? '/videos/live/new' : '/videos/upload';
        }
      }
      if (workflow) { title.required=false; return; }
      openSection(form, 'media');
      const mediaLabel = value === "audio" ? form.dataset.i18nAudioAdd : form.dataset.i18nMediaAdd;
      const dropzoneLabel = form.querySelector(".compose__dropzone-label");
      if (mediaLabel && dropzoneLabel) dropzoneLabel.firstChild.textContent = mediaLabel;
      show(titleFields, value !== "note" || !!link);
      title.required = value !== "note" || !!link?.value.trim();
      form.querySelectorAll("[data-compose-event-fields]").forEach((el) => show(el, value === "event"));
      form.querySelectorAll("[data-compose-note-only]").forEach((el) => show(el, value === "note"));
      form.querySelectorAll("[data-compose-audio-only]").forEach((el) => show(el, value === "audio"));
      form.querySelectorAll("[data-compose-article-only]").forEach((el) => show(el, value === "article"));
      form.querySelectorAll("[data-compose-schedulable]").forEach((el) => show(el, !["event", "audio"].includes(value)));
      form.querySelectorAll("[data-compose-compatibility]").forEach((el) => show(el, el.dataset.composeCompatibility === value));
      body.placeholder = body.dataset[`placeholder${value[0].toUpperCase()}${value.slice(1)}`] || body.dataset.placeholderNew;
      form.querySelectorAll("[data-compose-title-label], [data-compose-body-label]").forEach((el) => {
        el.textContent = el.dataset[value] || el.dataset.note;
        if (el.hasAttribute("data-compose-body-label")) el.classList.toggle("visually-hidden", value === "note");
      });
      body.rows = value === "article" ? 12 : 5;
      body.style.height = "auto";
      body.style.height = `${body.scrollHeight}px`;
    };
    kind.addEventListener("change", () => {
      // A fresh composer can open its publishing workflow immediately. Keep
      // a populated draft in place so changing type never throws it away.
      const populated = body.value.trim() || title.value.trim() ||
        form.querySelector('.compose__attachment, [name="reuse_media[]"]:checked');
      if (['video','live'].includes(kind.value) && !populated) {
        location.assign(kind.value === 'live' ? '/videos/live/new' : '/videos/upload');
        return;
      }
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

  // Discovery appends verified local catalogue pages. Keep the native link as
  // the no-script path; failed/redirected loads never start a retry loop.
  function bindDiscoveryImages(root) {
    root.querySelectorAll('.discovery-avatar > img').forEach(img => {
      if (img.dataset.bound) return;
      img.dataset.bound = '1';
      img.addEventListener('error', () => img.remove(), { once: true });
      if (img.complete && !img.naturalWidth) img.remove();
    });
  }
  function bindDiscovery(root) {
    bindDiscoveryImages(root);
    root.querySelectorAll('form[data-wider-search]').forEach(form => {
      if (form.dataset.bound) return;
      form.dataset.bound = '1';
      form.addEventListener('submit', () => {
        form.querySelector('button[type="submit"]').disabled = true;
        const message = form.querySelector('.discovery-wider__status');
        message.textContent = message.dataset.searching;
      });
      window.addEventListener('pageshow', () => {
        form.querySelector('button[type="submit"]').disabled = false;
        form.querySelector('.discovery-wider__status').textContent = '';
      });
    });
    const section = root.querySelector('.discovery');
    const results = section?.querySelector('[data-discovery-results]');
    const pager = section?.querySelector('.discovery-pagination');
    const link = pager?.querySelector('.discovery-more');
    if (!results || !link || section.dataset.pagingBound) return;
    section.dataset.pagingBound = '1';
    const status = pager.querySelector('.discovery-page-status');
    // No-JS has the normal next-page link. JS uses the same URL as its sentinel.
    link.hidden = 'IntersectionObserver' in window;
    const seen = new Set();
    let busy = false, failed = false, exhausted = false, emptyPages = 0;
    const localUrl = value => {
      const url = new URL(value, location.href);
      return url.origin === location.origin && ['/discover', '/discover/posts', '/discover/communities', '/discover/topics', '/discover/videos'].includes(url.pathname) ? url : null;
    };
    const observer = 'IntersectionObserver' in window ? new IntersectionObserver(entries => {
      if (entries.some(entry => entry.isIntersecting)) maybeLoad();
    }, { rootMargin: '200px' }) : null;
    function maybeLoad() {
      if (!section.isConnected) { observer?.disconnect(); return; }
      if (!failed && !exhausted && !document.hidden &&
          !pager.contains(document.activeElement) && pager.getBoundingClientRect().top <= innerHeight + 200) load(false);
    }
    async function load(manual) {
      if (busy || exhausted) return;
      const url = localUrl(link.href);
      // A repeated cursor or malformed response leaves normal navigation usable.
      if (!url || seen.has(url.href) || seen.size >= 100) { failed = true; link.hidden = false; return; }
      busy = true;
      link.setAttribute('aria-disabled', 'true');
      results.setAttribute('aria-busy', 'true');
      status.textContent = status.dataset.loading;
      try {
        const response = await fetch(url, { credentials: 'same-origin', cache: 'no-store',
          headers: { 'X-Requested-With': 'fetch' }, signal: AbortSignal.timeout(15000) });
        if (!response.ok || response.redirected) throw new Error('Unavailable page');
        const doc = new DOMParser().parseFromString(await response.text(), 'text/html');
        const incomingSection = doc.querySelector('.discovery');
        const incoming = incomingSection?.querySelector('[data-discovery-results]');
        if (!incoming || incoming.dataset.discoveryResults !== results.dataset.discoveryResults ||
            (section.dataset.discoveryView !== 'servers' && incomingSection.dataset.discoverySource !== section.dataset.discoverySource) ||
            incomingSection.dataset.discoveryQuery !== section.dataset.discoveryQuery ||
            incomingSection.dataset.discoveryChannel !== section.dataset.discoveryChannel ||
            incomingSection.querySelector('[data-discovery-topic]')?.dataset.discoveryTopic !== section.querySelector('[data-discovery-topic]')?.dataset.discoveryTopic) throw new Error('Different view');
        const ids = new Set([...results.children].map(node => node.id));
        const nodes = [...incoming.children].filter(node => node.id && !ids.has(node.id));
        const next = incomingSection.querySelector('.discovery-more');
        const nextUrl = next && localUrl(next.href);
        if (next && (!nextUrl || nextUrl.href === url.href || seen.has(nextUrl.href))) throw new Error('Repeated page');
        results.append(...nodes);
        bindDiscoveryImages(results);
        seen.add(url.href);
        emptyPages = nodes.length ? 0 : emptyPages + 1;
        failed = emptyPages >= 3;
        exhausted = !next;
        if (next) link.href = nextUrl.href;
        else { link.hidden = true; observer?.disconnect(); }
        status.textContent = exhausted ? status.dataset.end : status.dataset.loaded;
        if (manual) {
          const target = nodes[0]?.querySelector('h2') || status;
          target.setAttribute('tabindex', '-1');
          target.focus({ preventScroll: true });
          if (nodes.length) target.scrollIntoView({ block: 'nearest' });
        }
      } catch {
        failed = true;
        status.textContent = status.dataset.error;
        link.hidden = false;
      } finally {
        busy = false;
        link.removeAttribute('aria-disabled');
        results.removeAttribute('aria-busy');
      }
      // Empty/short pages may leave the sentinel intersecting without a new
      // observer event. At most three empty pages are followed automatically.
      if (!manual) requestAnimationFrame(maybeLoad);
    }
    link.addEventListener('click', event => {
      if (event.button || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      if (seen.has(link.href) || seen.size >= 100) return;
      event.preventDefault();
      if (!busy) { failed = false; emptyPages = 0; load(true); }
    });
    observer?.observe(pager);
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
      // A completed drag/swipe/pinch may synthesize one click. Suppress that
      // click only; a fresh pointer sequence must remain an immediate close.
      suppressGestureClick: false,
      // Opening the viewer adds one same-page history entry. This lets the
      // browser Back gesture close the viewer before leaving the page.
      historyActive: false,
      historyClosing: false,
      scrollRestoration: null,
      scrollRestorationTimer: null,
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
      if (box.suppressGestureClick) {
        box.suppressGestureClick = false;
        return;
      }
      if (event.target.closest(".lightbox__meta")) return;
      closeLightbox();
    });
    bindLightboxGestures(box);
    window.addEventListener("popstate", () => {
      if (!box.historyActive) return;
      box.historyActive = false;
      box.historyClosing = false;
      closeLightbox(true);
      // Native restoration follows popstate. Keep it disabled through that
      // traversal, then return ownership to the page (including a music dock
      // that already uses manual restoration). A fresh open cancels this task.
      box.scrollRestorationTimer = setTimeout(() => {
        history.scrollRestoration = box.scrollRestoration;
        box.scrollRestoration = null;
        box.scrollRestorationTimer = null;
      }, 0);
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
      clearTimeout(box.scrollRestorationTimer);
      box.scrollRestorationTimer = null;
      if (box.scrollRestoration === null) {
        box.scrollRestoration = history.scrollRestoration;
      }
      // Set the underlying entry's mode *before* pushing the viewer entry.
      // Otherwise closing can restore its old reply-anchor scroll position.
      history.scrollRestoration = "manual";
      history.pushState(history.state, "", location.href);
      box.historyActive = true;
    }
    box.el.hidden = false;
    document.body.style.overflow = "hidden";
    document.addEventListener("keydown", onLightboxKey, true);
    showLightboxItem(
      Number.isInteger(index) && index >= 0 && index < items.length ? index : 0,
    );
    box.close.focus({ preventScroll: true });
  }

  function closeLightbox(fromHistory = false) {
    const box = lightbox;
    if (!box || box.el.hidden) return;
    // All close paths finish in popstate, with restoration still disabled.
    // Keep the overlay until then and ignore duplicate closes so rapid input
    // cannot consume another page's history entry.
    if (box.historyActive && !fromHistory) {
      if (!box.historyClosing) {
        box.historyClosing = true;
        history.back();
      }
      return;
    }
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
    if (box.opener) box.opener.focus({ preventScroll: true });
    box.opener = null;
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
      // Any new pointer sequence is intentional, even if it starts while the
      // previous slide is settling. Do not let stale gesture suppression turn
      // that tap into a dead first attempt.
      box.suppressGestureClick = false;
      if (box.animating) {
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
      }
    });
    const release = (event) => {
      const p = pointers.get(event.pointerId);
      pointers.delete(event.pointerId);
      if (!p) return;
      if (moved || p.pinched) box.suppressGestureClick = true;
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
  // Legacy proxy URLs for large remote A/V can outlive the proxy's brief wait
  // and 404 while a whole-file cache job keeps running server-side. The media
  // element then fires an error; poll the proxy and retry once the cached copy
  // has landed. Current podcast URLs use the streaming sparse cache instead.
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
  let p2pLoading = null;
  const P2P_PREF = "plamenu:p2p-enabled";
  let p2pChoice = null;
  let pendingPeerPreference = null;
  let peerPreferenceBusy = false;

  function savePeerPreference(panel, enabled) {
    // One in-flight request and one latest choice, shared across every player.
    // Rapid toggles cannot build an unbounded queue or reorder saved choices.
    pendingPeerPreference = {panel, enabled};
    if (peerPreferenceBusy) return;
    peerPreferenceBusy = true;
    (async () => {
      while (pendingPeerPreference) {
        const next = pendingPeerPreference;
        pendingPeerPreference = null;
        try {
          const response = await fetch('/web/videos/p2p', {method:'POST',
            credentials:'same-origin',headers:{Accept:'application/json'},
            body:new URLSearchParams({csrf:next.panel.dataset.p2pCsrf,
              enabled:String(next.enabled)}),keepalive:true});
          if(!response.ok)throw new Error('Preference rejected');
          if(!pendingPeerPreference){
            document.body.dataset.p2pDefault=String(next.enabled);
            document.body.dataset.p2pConfigured='true';
            next.panel.querySelector('.media__p2p-status').textContent=next.panel.dataset.p2pSaved;
          }
        } catch {
          if(!pendingPeerPreference)next.panel.querySelector('.media__p2p-status').textContent=next.panel.dataset.p2pFailed;
        }
      }
      peerPreferenceBusy = false;
    })();
  }

  function peerSharingEnabled() {
    if (document.body.dataset.p2pLocked === 'true') return false;
    if (p2pChoice !== null) return p2pChoice;
    // Account settings (including a non-JS opt-out), or an explicit server-only
    // embed URL, outrank stale browser storage before any tracker is contacted.
    if (document.body.dataset.p2pDefault !== undefined) {
      if (document.body.dataset.p2pDefault === 'false') return false;
      if (document.body.dataset.p2pConfigured !== 'false') return true;
      // Preserve an older browser-only opt-out until the viewer deliberately
      // saves an account setting. A schema default is not consent to reset it.
    }
    try { return localStorage.getItem(P2P_PREF) !== "false"; }
    catch (_err) { return true; }
  }

  // A migrated browser-only opt-out must also be reflected by the settings
  // form before its first explicit account save, not just by the player.
  const viewerPeerSetting = document.querySelector('form[action="/web/videos/settings"] input[name="p2p_enabled"]');
  if (viewerPeerSetting && document.body.dataset.p2pConfigured === 'false') {
    viewerPeerSetting.checked = peerSharingEnabled();
  }

  function loadP2P(cb) {
    const engine = window.p2pml?.hlsjs?.HlsJsP2PEngine;
    if (engine) return cb(engine);
    if (p2pLoading) return p2pLoading.push(cb);
    p2pLoading = [cb];
    const script = document.createElement("script");
    script.src = "/assets/p2p-media-loader-hlsjs.min.js?v=4.0.0";
    const flush = (value) => {
      const queue = p2pLoading || [];
      p2pLoading = null;
      queue.forEach((fn) => fn(value));
    };
    script.onload = () => flush(window.p2pml?.hlsjs?.HlsJsP2PEngine || null);
    script.onerror = () => flush(null);
    document.head.appendChild(script);
  }

  // A published Video supplies its checksum manifest. Read
  // that *before* constructing a peer engine (which could contact a tracker).
  // Without a bounded, usable manifest, regular HTTP HLS remains available.
  async function peerHashes(url, live = false, requestTimeoutMs = 2500) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), requestTimeoutMs);
    try {
      const response = await fetch(url, { signal: controller.signal, mode: "cors",
        cache: live ? "no-store" : "default" });
      if (!response.ok || !response.body ||
          Number(response.headers.get("content-length") || 0) > 2 * 1024 * 1024) return null;
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let content = "";
      let size = 0;
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        size += value.byteLength;
        if (size > 2 * 1024 * 1024) {
          await reader.cancel();
          return null;
        }
        content += decoder.decode(value, { stream: true });
      }
      content += decoder.decode();
      const hashes = JSON.parse(content);
      return hashes && typeof hashes === "object" && !Array.isArray(hashes) ? hashes : null;
    } catch (_err) {
      return null;
    } finally {
      clearTimeout(timeout);
    }
  }

  function peerSegmentValidator(hashes, checksumUrl, live, diagnostic = null) {
    let currentHashes = hashes;
    let refreshPending = null;
    return async (url, byteRange, data) => {
      try {
        const proxied = new URL(url, location.href);
        let encodedOrigin = proxied.searchParams.get("u");
        if (!encodedOrigin) {
          const livePath = proxied.pathname.match(/^\/media\/hls\/\d+\/live\/([A-Za-z0-9_-]+)\.[A-Za-z0-9]+$/);
          encodedOrigin = livePath?.[1];
        }
        const padded = encodedOrigin?.replace(/-/g, "+").replace(/_/g, "/");
        const origin = padded
          ? new TextDecoder().decode(Uint8Array.from(atob(padded), (character) => character.charCodeAt(0)))
          : url;
        const filename = new URL(origin, location.href)
          .pathname.split("/").pop();
        const expectedHash = () => {
          const entry = Object.hasOwn(currentHashes, filename) ? currentHashes[filename] : null;
          if (!entry || typeof entry !== "object") return entry;
          const start = byteRange?.start ?? Number(proxied.searchParams.get("s"));
          const length = Number(proxied.searchParams.get("l"));
          const end = byteRange?.end ?? (start + length - 1);
          const key = `${start}-${end}`;
          return Object.hasOwn(entry, key) ? entry[key] : null;
        };
        let expected = expectedHash();
        for (let attempt = 0; live && typeof expected !== "string" && attempt < 6; attempt++) {
          await new Promise((resolve) => setTimeout(resolve, 500));
          if (!refreshPending) {
            refreshPending = peerHashes(checksumUrl, true).finally(() => { refreshPending = null; });
          }
          const updated = await refreshPending;
          if (updated) currentHashes = updated;
          expected = expectedHash();
        }
        if (typeof expected !== "string" || !/^[a-f0-9]{64}$/.test(expected)) {
          diagnostic?.({kind:'hash-unavailable',filename:filename.slice(0,128)});
          return false;
        }
        const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", data));
        const actual = Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join("");
        if(actual !== expected) diagnostic?.({kind:'digest-mismatch',filename:filename.slice(0,128),expected,actual,bytes:data.byteLength});
        return actual === expected;
      } catch (_err) {
        return false;
      }
    };
  }

  // The supported peer-storage interface keeps late async completion after
  // player destruction harmless and makes browser memory an explicit budget.
  // It does not change the PeerTube wire protocol or checksum validators.
  function peerMemoryStorage(limitMiB) {
    const cache = new Map();
    const limit = Math.max(1, Math.min(128, limitMiB)) * 1024 * 1024;
    let used = 0, closed = false, changed = null, position = 0, windowSeconds = 4;
    const key = (stream, id) => JSON.stringify([stream, id]);
    const remove = (entryKey, affected) => {
      const entry = cache.get(entryKey);
      if (!entry) return;
      cache.delete(entryKey);
      used -= entry.data.byteLength;
      affected.add(entry.stream);
    };
    const notify = affected => {
      if (!closed && changed) for (const stream of affected) changed(stream);
    };
    return {
      async initialize(_core, main) { windowSeconds = Math.max(4, main?.highDemandTimeWindow || 4); },
      onPlaybackUpdated(time) { if (!closed && Number.isFinite(time)) position = time; },
      onSegmentRequested() {},
      setSegmentChangeCallback(callback) { if (!closed) changed = callback; },
      clear(live, incoming = 0) {
        if (closed) return;
        const affected = new Set();
        if (live) for (const [entryKey, entry] of cache) {
          if (entry.end < position - windowSeconds) remove(entryKey, affected);
        }
        for (const entryKey of cache.keys()) {
          if (used + incoming <= limit && cache.size < 4096) break;
          remove(entryKey, affected);
        }
        notify(affected);
      },
      async storeSegment(_swarm, stream, id, data, start, end, _type, live) {
        if (closed || !data?.byteLength || data.byteLength > limit) return;
        const entryKey = key(stream, id);
        const affected = new Set();
        remove(entryKey, affected);
        this.clear(live, data.byteLength);
        if (closed) return;
        cache.set(entryKey, { stream, id, data, start, end });
        used += data.byteLength;
        affected.add(stream);
        notify(affected);
      },
      async getSegmentData(_swarm, stream, id) {
        if (closed) return undefined;
        return cache.get(key(stream, id))?.data;
      },
      hasSegment(_swarm, stream, id) { return !closed && cache.has(key(stream, id)); },
      getStoredSegmentIds(_swarm, stream) {
        return closed ? [] : Array.from(cache.values()).filter(entry => entry.stream === stream).map(entry => entry.id);
      },
      getUsage() { return { totalCapacity: limit / (1024 * 1024), usedCapacity: used / (1024 * 1024) }; },
      destroy() { closed = true; changed = null; cache.clear(); used = 0; },
    };
  }

  function liveSegmentTimeout(seconds) {
    return Number.isFinite(seconds) && seconds > 0
      ? Math.min(10000, Math.max(3000, Math.ceil(seconds * 1500))) : 3000;
  }

  function preservePeerPlaylistIdentity(hls) {
    const normalize = (_event, { details }) => {
      if (!details?.url) return;
      const url = new URL(details.url, location.href);
      // Delivery directives select a snapshot of the same rendition. The
      // peer engine keys its inventory by the master rendition's exact URL.
      // Preserve ordinary/auth query bytes rather than reserializing them.
      const parts = url.search.slice(1).split('&');
      const retained = parts.filter(part => {
        let key;
        try { key = decodeURIComponent(part.split('=', 1)[0].replace(/\+/g, ' ')); }
        catch (_err) { return true; }
        return !['_HLS_msn', '_HLS_part', '_HLS_skip'].includes(key);
      });
      if (retained.length === parts.length) return;
      url.search = retained.length ? '?' + retained.join('&') : '';
      details.url = url.href;
    };
    hls.on(hls.constructor.Events.LEVEL_UPDATED, normalize);
    hls.on(hls.constructor.Events.AUDIO_TRACK_LOADED, normalize);
  }

  async function peerOptions(video, live) {
    if (!peerSharingEnabled() || !window.RTCPeerConnection || !window.crypto?.subtle) return null;
    const uuid = video.dataset.p2pUuid;
    const swarmId = video.dataset.p2pSwarm;
    const checksumUrl = video.dataset.p2pSha256;
    let trackers;
    try { trackers = JSON.parse(video.dataset.p2pTrackers || "[]"); }
    catch (_err) { return null; }
    if (!uuid || !swarmId || !checksumUrl || !Array.isArray(trackers) || !trackers.length) return null;
    const startupDeadline = Date.now() + 5000;
    let hashes = await peerHashes(checksumUrl, live);
    // On-air is announced before the encoder commits its first complete
    // segment. A cold viewer must not permanently lose sharing to that brief
    // 404. Keep this startup retry finite and recheck opt-out before each fetch.
    for (let attempt = 0; live && !hashes && attempt < 5 && Date.now() < startupDeadline - 500; attempt++) {
      await new Promise(resolve => setTimeout(resolve, 500));
      if (!peerSharingEnabled()) return null;
      hashes = await peerHashes(checksumUrl, true, Math.min(2500, startupDeadline - Date.now()));
    }
    if (!hashes) return null;
    const validateSegment = peerSegmentValidator(hashes, checksumUrl, live,
      failure => { video.dataset.p2pIntegrityFailure = JSON.stringify(failure); });
    return {
      core: {
        announceTrackers: trackers,
        swarmId,
        streamSwarmIdBuilder: ({ peerProtocolVersion, streamType, properties }) => {
          const height = properties.height || Infinity;
          const width = properties.width || Infinity;
          const resolution = Math.min(height, width);
          return `pt-${peerProtocolVersion}-${uuid}-${streamType}-default-${Number.isFinite(resolution) ? resolution : "default"}`;
        },
        validateP2PSegment: validateSegment,
        // HTTP may finish a transfer that already contains peer-supplied
        // bytes. Validate the complete result at both completion boundaries.
        validateHTTPSegment: validateSegment,
        customSegmentStorageFactory: () => peerMemoryStorage(navigator.deviceMemory && navigator.deviceMemory <= 2 ? 32 : 64),
        isP2PUploadDisabled: navigator.connection?.type === "cellular" || !!navigator.connection?.saveData,
        highDemandTimeWindow: live ? 4 : 15,
        // Adaptive VOD startup can request an unseeded quality over HTTP.
        // The library's 3s no-byte default repeatedly cancels a slow origin
        // while switching levels, even when other peer bytes were verified.
        // Keep live's short recovery, but allow bounded 10s VOD HTTP startup.
        ...(live ? {} : { httpNotReceivingBytesTimeoutMs: 10000 }),
      },
      onHlsJsCreated: (hls) => {
        // This callback precedes the peer loader's event registration. Its
        // inventory must see the canonical metadata identity on every reload;
        // the actual HTTP request and parsed fragment URLs stay unchanged.
        if (live) preservePeerPlaylistIdentity(hls);
        video.dataset.p2pActive = "true";
        hls.p2pEngine.addEventListener("onStreamAdded", ({ stream }) => {
          if (stream.type === "main") video.dataset.p2pInfohash = stream.infoHash;
        });
        hls.p2pEngine.addEventListener("onPeerConnect", () => {
          video.dataset.p2pPeers = String(Number(video.dataset.p2pPeers || 0) + 1);
        });
        hls.p2pEngine.addEventListener("onChunkDownloaded", (bytes, source) => {
          if (source === "p2p") {
            video.dataset.p2pDownloaded = String(Number(video.dataset.p2pDownloaded || 0) + bytes);
          }
        });
        hls.p2pEngine.addEventListener("onSegmentLoaded", ({ downloadSource, bytesLength }) => {
          if (downloadSource === "p2p") {
            // This event fires only after the checksum validator accepted the
            // complete segment; chunk events alone are not proof of integrity.
            video.dataset.p2pValidated = String(Number(video.dataset.p2pValidated || 0) + bytesLength);
          }
        });
        hls.p2pEngine.addEventListener("onSegmentError", ({ error }) => {
          // Bounded local diagnostics; no peer IDs, addresses or URLs.
          video.dataset.p2pLastError = String(error?.type || "unknown").slice(0, 64);
          // A completed RTC transfer reports checksum refusal by closing the
          // peer; resumed transfers use the direct validation error instead.
          if (error?.type === "p2p-segment-validation-failed" ||
              (error?.type === "peer-closed" && error.cause?.type === "validation-failed")) {
            video.dataset.p2pValidationErrors = String(Number(video.dataset.p2pValidationErrors || 0) + 1);
          } else if (error?.type === "http-segment-validation-failed") {
            video.dataset.hlsHttpValidationErrors = String(Number(video.dataset.hlsHttpValidationErrors || 0) + 1);
          }
        });
        hls.p2pEngine.addEventListener("onChunkUploaded", (bytes) => {
          video.dataset.p2pUploaded = String(Number(video.dataset.p2pUploaded || 0) + bytes);
        });
      },
    };
  }
  function liveSeekTarget(video, hls) {
    const sync = hls?.liveSyncPosition;
    // The loaded seekable tail can still belong to a delayed/DVR transfer.
    // Ask HLS for its current target, not that stale buffer's last timestamp.
    if (Number.isFinite(sync)) return sync;
    return video.seekable.length ? video.seekable.end(video.seekable.length - 1) : null;
  }

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
  // the <select> once (or fills the VOD placeholder), then adds the audio
  // option once its track is known. Opening settings never loads the ladder.
  function buildQualityMenu(video, hls, Hls) {
    const figure = video.closest("figure") || video.parentNode;
    let select = figure.querySelector(".video-quality");
    const levels = hls.levels || [];
    // PeerTube ships separated audio as an EXT-X-MEDIA alternate track (its own
    // media playlist); hls.js resolves it to an absolute same-origin proxied url
    // we can play video-less.
    const audioUrl = ((hls.audioTracks || [])[0] || {}).url;
    if (!select || select.dataset.qualityPending === 'true') {
      if (!select && levels.length < 2 && !audioUrl) return; // native/live baseline
      if (!levels.length && !audioUrl) return; // keep the pending Auto row
      const fresh = !select;
      const masterUrl = video.getAttribute("data-hls");
      if (fresh) {
        select = document.createElement("select");
        select.className = "video-quality";
        select.setAttribute("aria-label", "Video quality");
      } else {
        select.replaceChildren();
        delete select.dataset.qualityPending;
        select.disabled = false;
      }
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
      const changeLevel = (level) => {
        // Before the first decoded buffer, an immediate switch aborts the
        // initial fragment and flushes tracks still being created. Let that
        // initialization complete and select the requested following load.
        if (video.readyState < 2 || !video.buffered.length) hls.loadLevel = level;
        else hls.currentLevel = level;
      };
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
            hls.loadLevel = level;
          });
          hls.loadSource(masterUrl);
        } else {
          changeLevel(level);
        }
      });
      hls.on(Hls.Events.LEVEL_SWITCHED, () => {
        if (!audioMode && hls.autoLevelEnabled) select.value = "-1";
      });
      if (fresh) {
        const settings = figure.querySelector(".vod-controls__settings");
        if (settings) appendVodSetting(settings, select);
        else figure.appendChild(select);
      }
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

  const hlsCleanup = new WeakMap();
  function hlsSubtitleError(data) {
    return data?.frag?.type === 'subtitle' || data?.context?.type === 'subtitleTrack'
      || data?.details?.startsWith('subtitleTrack') || data?.details?.startsWith('subtitleFrag');
  }
  function catchUpBufferedLive(video, hls, rewound) {
    if (rewound || video.paused || video.readyState < 2) return false;
    const target = hls.liveSyncPosition;
    const edge = hls.latestLevelDetails?.edge;
    if (!Number.isFinite(target) || !Number.isFinite(edge) ||
        target - video.currentTime <= .25) return false;
    for (let n = 0; n < video.buffered.length; n++) {
      if (video.buffered.start(n) <= target && target + .1 < video.buffered.end(n)) {
        video.currentTime = target;
        return true;
      }
    }
    return false;
  }
  function initHlsVideo(video) {
    const hlsUrl = video.getAttribute("data-hls");
    if (!hlsUrl) return;
    const goLive = video.closest("figure")?.querySelector(".media__go-live");
    // Keep the original control/listeners when its panel moves into a dialog.
    const choice = video.closest("figure")?.querySelector(".media__p2p-toggle");
    const progressive = video.getAttribute("src") || video.getAttribute("data-src");
    const original = video.getAttribute("data-original");
    let originalFallback = false;
    let hlsPlayer = null;
    let retired = false;
    let externalTrackModes = null;
    const trackKey = track => JSON.stringify([track.src, track.kind, track.srclang]);
    const rememberExternalTracks = () => {
      externalTrackModes = new Map([...video.querySelectorAll('track[src]')]
        .map(track => [trackKey(track), track.track.mode]));
    };
    video.addEventListener('plamenu:captions-choice', rememberExternalTracks);
    video.addEventListener('loadedmetadata', () => {
      if (!externalTrackModes) return;
      video.querySelectorAll('track[src]').forEach(track => {
        const mode = externalTrackModes.get(trackKey(track));
        if (mode !== undefined) track.track.mode = mode;
      });
    });
    // Opt-in numeric E2E observations; no engine control or URI export.
    const profiling = window.__ownedVideoProfile === true;
    let profileLevelCount = null;
    let profileSourceKind = 'not-started';
    let profileFallbackReason = 'none';
    let profileTargetDuration = null;
    const profileEvents = [];
    const profileIssue = (kind, fatal = false, data = null) => {
      if (!profiling) return;
      if (profileEvents.length >= 128) profileEvents.shift();
      const word = value => typeof value === 'string' && /^[A-Za-z0-9_-]{1,80}$/.test(value) ? value : null;
      profileEvents.push({ kind, fatal, atMs: performance.now(),
        time: video.currentTime, ready: video.readyState,
        ...(data ? {details:word(data.details),exceptionName:word(data.error?.name),
          level:Number.isInteger(data.frag?.level)?data.frag.level:null,
          sequence:Number.isInteger(data.frag?.sn)?data.frag.sn:null} : {}) });
    };
    if (profiling) {
      window.__ownedVideoProfiles ??= new WeakMap();
      window.__ownedVideoProfiles.set(video, () => ({
        hlsLevelCount: hlsPlayer?.levels?.length ?? profileLevelCount,
        playerFinalSourceKind: profileSourceKind,
        fallbackReason: profileFallbackReason,
        targetDurationSeconds: profileTargetDuration,
        hlsCurrentLevel: hlsPlayer?.currentLevel ?? null,
        hlsLiveSyncPositionSeconds: hlsPlayer?.liveSyncPosition ?? null,
        hlsLiveEdgeSeconds: hlsPlayer?.latestLevelDetails?.edge ?? null,
        events: profileEvents.slice(),
      }));
      for (const kind of ['waiting', 'stalled', 'seeking', 'seeked', 'playing', 'pause', 'ratechange', 'error']) {
        video.addEventListener(kind, () => profileIssue(kind));
      }
    }
    const liveVideo = video.dataset.live === 'live';
    let rewoundLive = false;
    let lastLiveTime = 0;
    // Live-edge playback must recover after a slow transfer instead of
    // retaining that delay forever. Deliberate DVR rewinds remain unbounded
    // by this edge-recovery rule and are never fast-forwarded behind the user.
    video.addEventListener('timeupdate', () => { lastLiveTime = video.currentTime; });
    video.addEventListener('seeking', () => {
      const previousTime = lastLiveTime;
      // HLS can set its initial edge before a decoded frame produces a
      // timeupdate. Remember that pending seek too, so a viewer's subsequent
      // backward seek is not compared with the old zero position.
      lastLiveTime = video.currentTime;
      if (!liveVideo || !hlsPlayer) return;
      const sync = hlsPlayer.liveSyncPosition;
      if (Number.isFinite(sync) && video.currentTime >= sync - 1) rewoundLive = false;
      else if (video.currentTime < previousTime - .5) rewoundLive = true;
      hlsPlayer.config.liveMaxLatencyDuration = rewoundLive ? Infinity : 5;
    });
    let vodPending = false;
    let vodTimer = null;
    let vodFallback = false;
    let adaptiveStarted = false;
    const clearVodWatch = () => { clearTimeout(vodTimer); vodTimer = null; };
    hlsCleanup.set(video, () => {
      retired = true; vodPending = false; clearVodWatch();
      hlsPlayer?.destroy(); hlsPlayer = null; video.pause();
    });
    const standardVideo = (reason = 'unclassified') => {
      if (retired || liveVideo || !progressive || (vodFallback && (!original || originalFallback))) return;
      const source = vodFallback ? original : progressive;
      if (vodFallback) originalFallback = true;
      vodFallback = true;
      if (profiling) {
        profileLevelCount = hlsPlayer?.levels?.length ?? profileLevelCount;
        profileSourceKind = 'standard-progressive';
        profileFallbackReason = reason;
      }
      const resume = Number.isFinite(video.currentTime) ? video.currentTime : 0;
      const playing = vodPending || !video.paused;
      vodPending = false;
      clearVodWatch();
      hlsPlayer?.destroy();
      hlsPlayer = null;
      delete video.dataset.p2pActive;
      video.dataset.p2pFallback = 'true';
      const figure = video.closest('figure');
      const quality = figure?.querySelector('.video-quality');
      if (quality === document.activeElement) video.focus();
      if (quality?.parentElement?.classList.contains('vod-controls__field')) quality.parentElement.remove();
      else quality?.remove();
      const sharing = choice;
      if (sharing) { sharing.checked = false; sharing.disabled = true; }
      if (figure && !figure.querySelector('.media__live-recovery')) {
        const notice = document.createElement('p');
        notice.className = 'media__live-recovery';
        notice.setAttribute('role','status');
        notice.textContent = video.dataset.vodFallback || 'Adaptive playback failed. Using the standard video player; peer sharing is off for this playback.';
        const link = document.createElement('a');
        link.href = source;
        link.textContent = video.dataset.vodBasic || 'Open standard video';
        const actions = document.createElement('span');
        actions.className = 'media__live-recovery-actions';
        actions.appendChild(link);
        notice.appendChild(actions);
        figure.appendChild(notice);
      }
      const recoveryLink = figure?.querySelector('.media__live-recovery a');
      if (recoveryLink) recoveryLink.href = source;
      video.addEventListener('loadedmetadata', () => {
        if (resume > 0) video.currentTime = Number.isFinite(video.duration) ? Math.min(resume, Math.max(0,video.duration-.1)) : resume;
        if (playing) video.play().catch(() => {});
      }, {once:true});
      video.src = source;
      reloadExternalTracks();
      video.load();
    };
    const watchVodStart = () => {
      clearVodWatch();
      if (retired || liveVideo || vodFallback || !progressive) return;
      const started = performance.now();
      const position = video.currentTime;
      let bufferedSince = null;
      const tick = () => {
        if (!video.isConnected || document.hidden || (!vodPending && video.paused)) return clearVodWatch();
        const frames = video.getVideoPlaybackQuality?.().totalVideoFrames;
        if ((frames === undefined || frames > 0) && video.currentTime > position+.05) return clearVodWatch();
        const elapsed = performance.now()-started;
        const noFrames = frames === 0 && video.readyState >= 2 && video.buffered.length;
        if (noFrames) bufferedSince ??= performance.now();
        else bufferedSince = null;
        // A populated buffer with no decoded frames is a decoder stall, not
        // a slow origin. Otherwise permit the measured slow-peer HTTP startup.
        if (elapsed >= 15000 || (bufferedSince !== null && performance.now()-bufferedSince >= 5000)) {
          standardVideo(elapsed >= 15000 ? 'startup-deadline' : 'buffered-without-decoded-frames');
          return;
        }
        vodTimer = setTimeout(tick,250);
      };
      tick();
    };
    video.addEventListener('play',watchVodStart);
    video.addEventListener('pause',() => { if (!vodPending) clearVodWatch(); });
    video.addEventListener('error',() => { if (adaptiveStarted) standardVideo('video-element-error'); });
    const showLiveFailure = () => {
      const figure = video.closest("figure");
      if (!figure || figure.querySelector(".media__live-recovery")) return;
      figure.classList.add("media--live-failed");
      const message = document.createElement("p");
      message.className = "media__live-recovery";
      message.setAttribute("role", "status");
      message.textContent = video.dataset.livePlaybackError || "Live playback was interrupted.";
      const actions = document.createElement("span");
      actions.className = "media__live-recovery-actions";
      if (progressive) {
        const basic = document.createElement("a");
        basic.href = progressive;
        basic.textContent = video.dataset.liveBasic || "Try basic stream";
        actions.appendChild(basic);
      }
      const refresh = document.createElement("a");
      refresh.href = window.location.href;
      refresh.textContent = video.dataset.liveRefresh || "Refresh broadcast status";
      refresh.addEventListener("click", (event) => {
        event.preventDefault();
        window.location.reload();
      });
      actions.appendChild(refresh);
      message.appendChild(actions);
      figure.appendChild(message);
      const bounds = video.getBoundingClientRect();
      if (bounds.bottom > 0 && bounds.top < window.innerHeight) {
        message.scrollIntoView({ block: "center" });
      }
    };
    if (choice && window.RTCPeerConnection && window.crypto?.subtle) {
      choice.checked = peerSharingEnabled();
      choice.closest(".media__p2p").hidden = false;
      if (document.body.dataset.p2pLocked === 'true') {
        choice.disabled = true;
        choice.closest('.media__p2p').querySelector('.media__p2p-status').textContent=choice.closest('.media__p2p').dataset.p2pLocked;
      }
      choice.addEventListener("change", () => {
        p2pChoice = choice.checked;
        const panel=choice.closest('.media__p2p');
        const csrf=panel.dataset.p2pCsrf;
        if (csrf) {
          savePeerPreference(panel,choice.checked);
        } else {
          try { localStorage.setItem(P2P_PREF, choice.checked ? "true" : "false"); }
          catch (_err) { /* The choice still applies to this playback. */ }
        }
        document.querySelectorAll(".media__p2p-toggle").forEach((other) => {
          other.checked = other.disabled ? false : choice.checked;
        });
        document.querySelectorAll('video[data-hls]').forEach(other => other.dispatchEvent(new Event('plamenu:p2p-choice')));
        updateVideoEmbedChoices();
      });
      // Notify every mounted player, not just the one whose checkbox changed.
      // Listener ownership stays on the video so removed timeline cards can
      // be collected rather than accumulating document-level closures.
      video.addEventListener('plamenu:p2p-choice', () => {
        if (hlsPlayer?.p2pEngine) {
          hlsPlayer.p2pEngine.applyDynamicConfig({
            core: { isP2PDisabled: !peerSharingEnabled(), isP2PUploadDisabled: !peerSharingEnabled() },
          });
        }
      });
    }
    if (goLive) {
      const liveEdge = () => {
        return liveSeekTarget(video, hlsPlayer);
      };
      const updateGoLive = () => {
        const edge = liveEdge();
        goLive.hidden = !rewoundLive && (edge === null || edge - video.currentTime < 4);
      };
      goLive.addEventListener("click", () => {
        rewoundLive = false;
        if (hlsPlayer) hlsPlayer.config.liveMaxLatencyDuration = 5;
        const edge = liveEdge();
        if (edge !== null) {
          lastLiveTime = edge;
          video.currentTime = edge;
        }
        if (video.paused) video.play().catch(() => {});
        updateGoLive();
      });
      ["timeupdate", "seeking", "seeked", "durationchange", "loadedmetadata"].forEach((event) =>
        video.addEventListener(event, updateGoLive),
      );
    }
    // The progressive proxy is a real `src` in server-rendered HTML so script
    // blockers and CSP failures retain a working player. Once JS is running,
    // preserve it as the fallback. It must remain attached until the viewer
    // presses play: with no source the browser disables its native play button,
    // so the very `play` event that bootstraps hls.js can never happen.
    const native = video.canPlayType("application/vnd.apple.mpegurl");
    // Calling video.load() to cancel the progressive request also cancels an
    // already-loading external WebVTT track in Chromium. Replacing only our
    // ordinary <track src> nodes after HLS attaches starts their fetch again;
    // preserve the viewer's selected mode rather than forcing captions on.
    const reloadExternalTracks = () => {
      video.querySelectorAll("track[src]").forEach((track) => {
        const mode = externalTrackModes?.get(trackKey(track)) ?? track.track.mode;
        const fresh = track.cloneNode(true);
        // A stale default attribute can reselect the first language after
        // media.load(), overriding a choice made before HLS attaches.
        if (['captions', 'subtitles'].includes(track.kind)) fresh.default = mode === 'showing';
        track.replaceWith(fresh);
        fresh.track.mode = mode;
      });
    };
    video.addEventListener(
      "play",
      function onPlay() {
        if (retired) return;
        if (externalTrackModes === null) rememberExternalTracks();
        adaptiveStarted = true;
        // The progressive source existed only to keep the native control
        // actionable. Stop its just-started request before selecting HLS, so a
        // click never downloads both the MP4 fallback and the HLS ladder.
        vodPending = !liveVideo;
        video.pause();
        video.removeAttribute("src");
        video.load();
        watchVodStart();
        // hls.js is the ONLY player that handles PeerTube's multi-rendition
        // ladder + separated-audio alternate track, so it is preferred wherever
        // MediaSource exists (Chrome/Firefox/desktop Safari) — Chrome's *native*
        // HLS silently drops the alternate audio. Only iOS / old Safari (native
        // HLS, no MSE) play directly, skipping the hls.js download.
        if (!window.MediaSource && native) {
          vodPending = false;
          if (profiling) profileSourceKind = 'native-hls';
          video.src = hlsUrl;
          reloadExternalTracks();
          video.play().catch(() => {});
          return;
        }
        loadHls((Hls) => {
          if (retired || vodFallback) return;
          if (Hls && Hls.isSupported()) {
            // A live stream has no beginning to seek to and a window that
            // moves out from under a stalled player, so it starts at the live
            // edge and is told to chase it back after a buffer hole rather
            // than sitting where it fell behind.
            const live = video.dataset.live === "live";
            const hlsConfig = live
              ? {
                  capLevelToPlayerSize: true,
                  lowLatencyMode: true,
                  // A playlist can advertise a closed segment before its
                  // bytes reach MSE. Keep catch-up inside the decoded buffer;
                  // seeking to its empty end repeatedly stops healthy live.
                  liveSyncMode: 'buffered',
                  // Use seconds, not segment counts: compatible origins may
                  // publish four-second segments instead of our two seconds.
                  liveSyncDuration: 4,
                  liveMaxLatencyDuration: 5,
                  // A recovered stall must not permanently add another
                  // segment to the live-edge target. Explicit DVR rewinds
                  // use the separate unbounded-latency intent above.
                  liveSyncOnStallIncrease: 0,
                  // The origin deletes segments as the window slides; a
                  // player that keeps retrying a gone segment never
                  // recovers, so let it jump the gap instead.
                  maxBufferHole: 1,
                }
              : { capLevelToPlayerSize: true };
            // The peer module is lazy, and its constructor is reached only
            // after the viewer had a chance to opt out and checksums loaded.
            // A failed peer setup must not block ordinary HLS playback.
            const startHls = (options, Engine) => {
              if (retired || vodFallback) return;
              let hls;
              try {
                const Player = options && Engine ? Engine.injectMixin(Hls) : Hls;
                hls = new Player(options ? { ...hlsConfig, p2p: options } : hlsConfig);
              } catch (_err) {
                hls = new Hls(hlsConfig);
              }
            hlsPlayer = hls;
            if (profiling) {
              profileSourceKind = 'adaptive-hls';
              hls.on(Hls.Events.ERROR, (_event, data) => {
                const kind = data?.type === Hls.ErrorTypes.NETWORK_ERROR ? 'hls-network-error'
                  : data?.type === Hls.ErrorTypes.MEDIA_ERROR ? 'hls-media-error' : 'hls-other-error';
                profileIssue(kind, !!data?.fatal, data);
              });
              hls.on(Hls.Events.LEVEL_UPDATED, (_event, data) => {
                profileTargetDuration = Number.isFinite(data?.details?.targetduration) ? data.details.targetduration : null;
              });
            }
            hls.loadSource(hlsUrl);
            hls.attachMedia(video);
            hls.on(Hls.Events.MANIFEST_PARSED, () => {
              vodPending = false;
              if (profiling) profileLevelCount = hls.levels.length;
              buildQualityMenu(video, hls, Hls);
              reloadExternalTracks();
              video.play().catch(() => {});
            });
            hls.on(Hls.Events.ERROR, (_event, data) => {
              if (!data?.fatal || !hlsSubtitleError(data)) return;
              // hls.js can stop every loader after a subtitle HTTP failure.
              // Disable that track and resume media loading after its error
              // handlers finish, keeping the buffers, time and pause state.
              queueMicrotask(() => {
                if (retired || vodFallback || hlsPlayer !== hls) return;
                hls.subtitleTrack = -1;
                hls.startLoad(-1);
              });
            });
            if (!live) {
              hls.on(Hls.Events.ERROR, (_event,data) => {
                if (hlsSubtitleError(data)) return;
                if (data?.fatal) standardVideo(data.type === Hls.ErrorTypes.NETWORK_ERROR
                  ? 'fatal-hls-network' : data.type === Hls.ErrorTypes.MEDIA_ERROR ? 'fatal-hls-media' : 'fatal-hls-other');
              });
            }
            if (live) {
              // hls.js checks live sync when the playlist arrives. A safe
              // buffered target may become available only after that check;
              // converge to that selected target after the real bytes append
              // rather than retaining the extra maximum-latency slack.
              hls.on(Hls.Events.FRAG_BUFFERED, () => {
                catchUpBufferedLive(video, hls, rewoundLive);
              });
              hls.on(Hls.Events.LEVEL_UPDATED, (_event, data) => {
                hls.p2pEngine?.applyDynamicConfig({ core: {
                  httpNotReceivingBytesTimeoutMs: liveSegmentTimeout(Number(data?.details?.targetduration)),
                } });
              });
              // A live playlist can hiccup — a segment rolls out of the
              // window mid-fetch — and that is worth resuming from. A
              // broadcast that has *ended* looks the same at first and never
              // recovers, so the retries are counted and the player gives up
              // rather than hammering an origin that has nothing left to
              // serve. The page then shows the ended state on its next load.
              let recoveries = 0;
              let recoveryDeadline = null;
              let failed = false;
              const stopAfterFailedRecovery = () => {
                if (failed) return;
                failed = true;
                clearTimeout(recoveryDeadline);
                hls.destroy();
                if (goLive) goLive.hidden = true;
                showLiveFailure();
              };
              hls.on(Hls.Events.FRAG_BUFFERED, () => {
                recoveries = 0;
                clearTimeout(recoveryDeadline);
                recoveryDeadline = null;
              });
              hls.on(Hls.Events.ERROR, (_evt, data) => {
                if (failed || !data || hlsSubtitleError(data)) return;
                // A live playlist can keep loading successfully while every
                // segment fails. hls.js calls these errors non-fatal and may
                // retry forever, so bound the time to a buffered fragment.
                if (data.type === Hls.ErrorTypes.NETWORK_ERROR) {
                  recoveryDeadline ||= setTimeout(stopAfterFailedRecovery, 30000);
                }
                if (!data.fatal) return;
                if (data.type === Hls.ErrorTypes.NETWORK_ERROR && recoveries < 3) {
                  recoveries += 1;
                  // startLoad cannot retry a master that never parsed: there
                  // are no levels to start. A failed fragment can resume in
                  // place without throwing away the viewer's DVR position.
                  if (hls.levels.length) hls.startLoad();
                  else hls.loadSource(hlsUrl);
                  return;
                }
                stopAfterFailedRecovery();
              });
            }
            // Separated audio (hls.audioTracks) is only known after
            // MANIFEST_PARSED — refresh so the "Audio only" entry appears.
            hls.on(Hls.Events.AUDIO_TRACKS_UPDATED, () => {
              buildQualityMenu(video, hls, Hls);
            });
            };
            peerOptions(video, live).then((options) => {
              if (retired || vodFallback) return;
              if (!options || !peerSharingEnabled()) return startHls(null, null);
              loadP2P((Engine) => startHls(Engine && peerSharingEnabled() ? options : null, Engine));
            }).catch(() => startHls(null, null));
          } else if (native) {
            vodPending = false;
            if (profiling) profileSourceKind = 'native-hls';
            video.src = hlsUrl; // native HLS (Safari/iOS)
            reloadExternalTracks();
            video.play().catch(() => {});
          } else if (progressive) {
            if (!liveVideo) standardVideo('adaptive-player-unavailable');
            else {
              if (profiling) profileSourceKind = 'live-progressive-remux';
              video.src = progressive; // last-resort live remux playback
              reloadExternalTracks();
              video.play().catch(() => {});
            }
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

  // Progressive enhancement owns only the chrome. The media element and HLS
  // engine remain the source of playback, caption, history and quality state.
  const vodControllers = new Map();
  let vodObserver;
  function appendVodSetting(settings, control) {
    const field = document.createElement('label');
    field.className = 'vod-controls__field';
    const name = document.createElement('span');
    name.textContent = control.getAttribute('aria-label');
    field.append(name, control); settings.append(field);
    return field;
  }
  function bindVodControls(root) {
    root.querySelectorAll('[data-vod-controls]').forEach(gallery => {
      const figure = gallery.closest('figure');
      const video = figure?.querySelector('video');
      if (!video || vodControllers.has(video)) return;
      const isLive = video.dataset.live === 'live';
      let board;
      try {
        board = JSON.parse(gallery.dataset.vodControls);
        if (!board.labels || Object.values(board.labels).some(label => typeof label !== 'string')) return;
        if (board.url) {
          board.url = new URL(board.url, location.href);
          if (board.url.origin !== location.origin || !['http:', 'https:'].includes(board.url.protocol)
            || !Number.isInteger(board.columns) || board.columns < 1 || board.columns > 10
            || !Number.isInteger(board.rows) || board.rows < 1 || board.rows > 10
            || !Number.isInteger(board.count) || board.count < 1 || board.count > 100
            || board.count > board.columns * board.rows
            || !Number.isInteger(board.interval) || board.interval < 1 || board.interval > 604800) board.url = null;
        }
        if (!Number.isFinite(board.duration) || board.duration <= 0 || board.duration > 604800) board.duration = 0;
      } catch { return; }
      const owner = new AbortController();
      const listen = (target, kind, handler) => target.addEventListener(kind, handler, {signal: owner.signal});
      const labels = board.labels;
      const element = (tag, className) => {
        const node = document.createElement(tag);
        if (className) node.className = className;
        return node;
      };
      const icons = {
        play: '<path d="m8 5 11 7-11 7Z" fill="currentColor" stroke="none"/>',
        pause: '<path d="M8 5v14M16 5v14" stroke-width="4"/>',
        sound: '<path d="m11 5-6 4H2v6h3l6 4Z"/><path d="M15 8a6 6 0 0 1 0 8m3-11a10 10 0 0 1 0 14"/>',
        muted: '<path d="m11 5-6 4H2v6h3l6 4Z"/><path d="m16 9 6 6m0-6-6 6"/>',
        fullscreen: '<path d="M8 3H3v5m13-5h5v5M3 16v5h5m13-5v5h-5"/>',
        settings: '<path d="m9 3-.6 2.2-2 .9-2-.6-2 3.5 1.5 1.6v2.8L2.4 15l2 3.5 2-.6 2 .9L9 21h6l.6-2.2 2-.9 2 .6 2-3.5-1.5-1.6v-2.8L21.6 9l-2-3.5-2 .6-2-.9L15 3Z"/><circle cx="12" cy="12" r="3"/>',
        pip: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M12 12h6v5h-6Z" fill="currentColor" stroke="none"/>',
      };
      const setIcon = (node, icon) => { node.innerHTML = svgIcon(icons[icon]); };
      const button = (label, icon) => {
        const node = element('button');
        node.type = 'button'; setIcon(node, icon);
        node.setAttribute('aria-label', label); node.title = label;
        return node;
      };
      const wrapper = element('div', 'vod-player');
      const controls = element('div', 'vod-controls');
      const timeline = element('div', 'vod-controls__timeline');
      const seek = element('input');
      seek.type = 'range'; seek.min = '0'; seek.max = String(board.duration); seek.step = '0.1'; seek.value = '0';
      seek.setAttribute('aria-label', labels.seek);
      const preview = element('div', 'vod-controls__preview'); preview.hidden = true; preview.setAttribute('aria-hidden', 'true');
      const tile = element('span', 'vod-controls__tile');
      const image = element('img'); image.alt = ''; image.width = (board.columns || 1) * 160; image.height = (board.rows || 1) * 90; tile.hidden = !board.url;
      const previewTime = element('span'); tile.append(image); preview.append(tile, previewTime); timeline.append(preview, seek);
      const row = element('div', 'vod-controls__row');
      const play = button(labels.play, 'play'); play.className = 'vod-controls__play';
      const time = element('span', 'vod-controls__time'); time.setAttribute('aria-hidden', 'true');
      const mute = button(labels.mute, 'sound');
      const volume = element('input'); volume.type = 'range'; volume.min = '0'; volume.max = '1'; volume.step = '.05';
      volume.setAttribute('aria-label', labels.volume); volume.className = 'vod-controls__volume';
      const captions = element('select'); captions.setAttribute('aria-label', labels.captions);
      const fullscreen = button(labels.fullscreen, 'fullscreen');
      const settings = element('details', 'vod-controls__menu');
      const summary = element('summary'); setIcon(summary, 'settings');
      summary.setAttribute('aria-label', labels.settings); summary.title = labels.settings;
      const summaryText = element('span', 'visually-hidden'); summaryText.textContent = labels.settings; summary.append(summaryText);
      const options = element('div', 'vod-controls__settings');
      const heading = element('p', 'vod-controls__heading'); heading.textContent = labels.settings; options.append(heading);
      let quality = figure.querySelector('.video-quality');
      if (!quality && video.dataset.hls) {
        quality = element('select', 'video-quality'); quality.setAttribute('aria-label', 'Video quality');
        quality.add(new Option('Auto', '-1')); quality.value = '-1'; quality.disabled = true;
        quality.dataset.qualityPending = 'true';
      }
      if (quality) appendVodSetting(options, quality);
      const settingsVolume = volume.cloneNode(); settingsVolume.className = 'vod-controls__volume-setting';
      const volumeField = appendVodSetting(options, settingsVolume); volumeField.classList.add('vod-controls__field--volume');
      const speed = element('select'); speed.setAttribute('aria-label', labels.speed);
      [.5, .75, 1, 1.25, 1.5, 2].forEach(value => speed.add(new Option(`${value}×`, String(value))));
      speed.value = String(video.playbackRate);
      const speedField = appendVodSetting(options, speed);
      speedField.hidden = isLive;
      const captionField = appendVodSetting(options, captions);
      const standardPip = document.pictureInPictureEnabled && video.requestPictureInPicture;
      const webkitPip = video.webkitSupportsPresentationMode && video.webkitSetPresentationMode;
      let pip;
      if (standardPip || webkitPip) {
        pip = button(labels.pip, 'pip'); pip.className = 'vod-controls__pip';
        const pipText = element('span'); pipText.textContent = labels.pip; pip.append(pipText); options.append(pip);
        listen(pip, 'click', async () => {
          try {
            if (standardPip) {
              if (document.pictureInPictureElement === video) await document.exitPictureInPicture();
              else await video.requestPictureInPicture();
            } else video.webkitSetPresentationMode(video.webkitPresentationMode === 'picture-in-picture' ? 'inline' : 'picture-in-picture');
          } catch { /* The platform may deny this request. */ }
        });
      }
      // Keep the existing MP4 escape hatch reachable without dedicating a
      // second row to it. The original paragraph returns on native fallback.
      const standardLink = video.dataset.original && Array.from(figure.querySelectorAll(':scope > p > a[href]'))
        .find(link => link.getAttribute('href') === video.dataset.src);
      const standard = standardLink?.parentElement;
      const standardSlot = standard && document.createComment('standard video');
      if (standard) {
        standard.before(standardSlot); standard.classList.add('vod-controls__standard'); options.append(standard);
      }
      settings.append(summary, options);
      row.append(play, time, mute, volume, settings, fullscreen); controls.append(timeline, row);
      const goLive = isLive && figure.querySelector('.media__go-live');
      const liveSlot = goLive && document.createComment('live edge control');
      if (goLive) { goLive.before(liveSlot); time.after(goLive); }
      wrapper.classList.toggle('vod-player--live', isLive);
      const placeSettings = () => {
        if (!settings.open) return;
        const bounds = controls.getBoundingClientRect();
        const above = bounds.top + 16 - 8;
        const below = innerHeight - bounds.bottom - 8;
        // Narrow cards may have less video height than the settings panel.
        // Use the side with room and scroll the panel on short viewports.
        const placement = above >= options.scrollHeight || above >= below ? 'above' : 'below';
        settings.dataset.placement = placement;
        options.style.maxBlockSize = `${Math.max(44, placement === 'above' ? above : below)}px`;
      };
      listen(settings, 'toggle', placeSettings);
      listen(window, 'resize', () => { hidePreview(); placeSettings(); });
      listen(document, 'click', event => {
        if (settings.open && !settings.contains(event.target)) settings.open = false;
      });
      listen(settings, 'keydown', event => {
        if (event.key === 'Escape' && settings.open) {
          event.preventDefault(); event.stopPropagation(); settings.open = false; summary.focus();
        }
      });
      let duration = board.duration;
      let rangeStart = 0;
      let dragging = false;
      let pointer = null;
      let pending = null;
      let pointerCommit = null;
      let spriteUrl = null;
      let spriteLoading = false;
      let disposed = false;
      let playIcon, muteIcon;
      const format = seconds => {
        seconds = Math.max(0, Math.floor(seconds || 0));
        const minutes = Math.floor(seconds / 60);
        return minutes >= 60 ? `${Math.floor(minutes / 60)}:${String(minutes % 60).padStart(2, '0')}:${String(seconds % 60).padStart(2, '0')}`
          : `${minutes}:${String(seconds % 60).padStart(2, '0')}`;
      };
      const clamp = value => Math.min(duration, Math.max(rangeStart, Number(value) || 0));
      const release = () => {
        const captured = pointer;
        pointer = null; dragging = false;
        if (captured !== null && seek.hasPointerCapture(captured)) seek.releasePointerCapture(captured);
      };
      const sync = () => {
        if (disposed) return;
        if (isLive) {
          const ranges = video.seekable;
          rangeStart = ranges.length ? ranges.start(ranges.length - 1) : 0;
          duration = ranges.length ? ranges.end(ranges.length - 1) : 0;
          seek.min = String(rangeStart); seek.max = String(duration);
          timeline.hidden = duration - rangeStart <= 1 || Number(video.dataset.liveDvr) === 0;
        }
        if (!dragging && pending === null) seek.value = String(clamp(video.currentTime));
        const position = dragging ? Number(seek.value) : pending ?? video.currentTime;
        seek.disabled = duration <= rangeStart;
        const behind = Math.max(0, duration - position);
        const timeLabel = isLive ? (behind > 4 ? `−${format(behind)}` : (labels.live || 'Live')) : `${format(position)} / ${format(duration)}`;
        seek.setAttribute('aria-valuetext', timeLabel);
        seek.style.setProperty('--vod-progress', `${duration > rangeStart ? (clamp(position) - rangeStart) / (duration - rangeStart) * 100 : 0}%`);
        time.textContent = timeLabel;
        time.classList.toggle('vod-controls__time--live', isLive && behind <= 4);
        const nextPlayIcon = video.paused ? 'play' : 'pause';
        if (playIcon !== nextPlayIcon) { setIcon(play, nextPlayIcon); playIcon = nextPlayIcon; }
        play.setAttribute('aria-label', video.paused ? labels.play : labels.pause); play.title = play.getAttribute('aria-label');
        const nextMuteIcon = video.muted || video.volume === 0 ? 'muted' : 'sound';
        if (muteIcon !== nextMuteIcon) { setIcon(mute, nextMuteIcon); muteIcon = nextMuteIcon; }
        mute.setAttribute('aria-label', video.muted ? labels.unmute : labels.mute); mute.title = mute.getAttribute('aria-label');
        mute.setAttribute('aria-pressed', String(video.muted)); volume.value = String(video.muted ? 0 : video.volume);
        volume.style.setProperty('--vod-progress', `${Number(volume.value) * 100}%`);
        settingsVolume.value = volume.value;
        settingsVolume.style.setProperty('--vod-progress', `${Number(volume.value) * 100}%`);
        speed.value = String(video.playbackRate);
        fullscreen.setAttribute('aria-pressed', String(document.fullscreenElement === wrapper || video.webkitDisplayingFullscreen === true));
        if (pip) {
          pip.disabled = video.readyState < HTMLMediaElement.HAVE_METADATA || video.videoWidth === 0
            || (!standardPip && !video.webkitSupportsPresentationMode('picture-in-picture'));
          pip.setAttribute('aria-pressed', String(document.pictureInPictureElement === video || video.webkitPresentationMode === 'picture-in-picture'));
        }
      };
      const hidePreview = () => { preview.hidden = true; };
      // Fetch at first preview intent, never on card render. Read incrementally
      // so even a broken response cannot allocate an unbounded sprite buffer.
      const loadSprite = async () => {
        if (!board.url || spriteLoading || spriteUrl || disposed) return;
        spriteLoading = true;
        try {
          const response = await fetch(board.url, {signal: owner.signal, credentials: 'same-origin', redirect: 'error'});
          if (!response.ok || !response.headers.get('content-type')?.startsWith('image/jpeg')) throw new Error('Invalid storyboard');
          const reader = response.body.getReader(); const chunks = []; let size = 0;
          try {
            while (true) {
              const {value, done} = await reader.read(); if (done) break;
              size += value.byteLength;
              if (size > 2 * 1024 * 1024) { await reader.cancel(); throw new Error('Storyboard exceeds limit'); }
              chunks.push(value);
            }
          } finally { reader.releaseLock(); }
          if (disposed) return;
          spriteUrl = URL.createObjectURL(new Blob(chunks, {type: 'image/jpeg'})); image.src = spriteUrl;
          await image.decode();
          if (image.naturalWidth !== board.columns * 160 || image.naturalHeight !== board.rows * 90) throw new Error('Invalid storyboard dimensions');
        } catch {
          if (spriteUrl) URL.revokeObjectURL(spriteUrl);
          spriteUrl = null; board.url = null; tile.hidden = true;
        }
      };
      const showPreview = seconds => {
        if (disposed || duration <= 0) return;
        seconds = clamp(seconds);
        const index = Math.min(board.count - 1, Math.floor(seconds / board.interval));
        if (board.url) image.style.transform = `translate(${-index % board.columns * 160}px, ${-Math.floor(index / board.columns) * 90}px)`;
        previewTime.textContent = isLive ? `−${format(duration - seconds)}` : format(seconds);
        // Clamp within the control surface even at 320px and either endpoint.
        const width = timeline.clientWidth;
        preview.style.left = `clamp(80px, ${(seconds - rangeStart) / (duration - rangeStart) * width}px, calc(100% - 80px))`;
        preview.hidden = false; loadSprite();
      };
      const commit = value => {
        pending = clamp(value); seek.value = String(pending); hidePreview(); sync();
        if (video.readyState >= HTMLMediaElement.HAVE_METADATA) {
          video.currentTime = pending; pending = null; sync();
        } else if (video.dataset.hls) video.play().catch(() => {});
        else video.load();
      };
      const togglePlayback = () => { if (video.paused) video.play().catch(() => {}); else video.pause(); };
      let chromeTimer;
      let controlsHovered = false;
      const fullscreenActive = () => document.fullscreenElement === wrapper;
      const showChrome = () => {
        clearTimeout(chromeTimer);
        wrapper.classList.remove('vod-player--idle');
        if (!fullscreenActive() || video.paused || video.ended) return;
        chromeTimer = setTimeout(() => {
          if (!video.paused && !settings.open && !dragging && !controlsHovered
            && !controls.querySelector(':focus-visible')) wrapper.classList.add('vod-player--idle');
        }, 2500);
      };
      let touchChromeIdle = null;
      listen(wrapper, 'pointerdown', event => { if(event.pointerType === 'touch' && event.target === video && fullscreenActive()) touchChromeIdle=wrapper.classList.contains('vod-player--idle'); });
      listen(wrapper, 'pointercancel', () => { touchChromeIdle=null; });
      listen(wrapper, 'pointermove', event => { if(event.pointerType !== 'touch') showChrome(); });
      listen(wrapper, 'focusin', showChrome);
      listen(wrapper, 'keydown', showChrome);
      listen(controls, 'pointerenter', event => { if (event.pointerType === 'mouse') controlsHovered = true; });
      listen(controls, 'pointerleave', () => { controlsHovered = false; showChrome(); });
      listen(settings, 'toggle', showChrome);
      // Looping/rebuffering emits playing without a user interaction. It must
      // not continuously reset the inactivity timer.
      ['play', 'pause', 'ended'].forEach(kind => listen(video, kind, showChrome));
      listen(play, 'click', togglePlayback);
      listen(video, 'click', event => {
        if (fullscreenActive() && event.pointerType === 'touch') {
          if (touchChromeIdle ?? wrapper.classList.contains('vod-player--idle')) showChrome();
          else if (!video.paused) { clearTimeout(chromeTimer); wrapper.classList.add('vod-player--idle'); }
          touchChromeIdle=null;
          return;
        }
        if (!video.controls && !event.defaultPrevented && event.button === 0) togglePlayback();
      });
      listen(video, 'keydown', event => {
        if (video.controls || event.defaultPrevented || event.target !== video
          || event.altKey || event.ctrlKey || event.metaKey || ![' ', 'Enter'].includes(event.key)) return;
        event.preventDefault();
        if (!event.repeat) togglePlayback();
      });
      listen(mute, 'click', () => { video.muted = !video.muted; });
      listen(volume, 'input', () => { video.volume = Number(volume.value); video.muted = video.volume === 0; });
      listen(settingsVolume, 'input', () => { video.volume = Number(settingsVolume.value); video.muted = video.volume === 0; });
      listen(speed, 'change', () => { video.playbackRate = Number(speed.value); });
      const pointerPosition = event => {
        const bounds = seek.getBoundingClientRect();
        return clamp(rangeStart + (event.clientX - bounds.left) / bounds.width * (duration - rangeStart));
      };
      listen(seek, 'pointerdown', event => {
        if (event.button !== 0 || dragging) return;
        // Own the pointer position as well as capture. WebKit's native range
        // drag stops updating its value when script owns pointer capture.
        event.preventDefault(); seek.focus({preventScroll: true});
        pointerCommit = null; dragging = true; pointer = event.pointerId;
        seek.setPointerCapture(pointer); seek.value = String(pointerPosition(event));
        showPreview(Number(seek.value)); sync();
      });
      listen(seek, 'pointermove', event => {
        if (event.pointerType === 'touch' && !dragging) return;
        if (dragging && event.pointerId !== pointer) return;
        if (dragging) { seek.value = String(pointerPosition(event)); sync(); }
        showPreview(dragging ? Number(seek.value) : pointerPosition(event));
      });
      listen(seek, 'input', () => { showPreview(Number(seek.value)); if (dragging) sync(); });
      listen(seek, 'pointerup', event => { if (dragging && event.pointerId === pointer) {
        seek.value = String(pointerPosition(event));
        const value = seek.value; pointerCommit = value; release(); commit(value);
      } });
      listen(seek, 'pointercancel', event => { if (event.pointerId === pointer) {
        release(); hidePreview(); sync(); pointerCommit = seek.value;
      } });
      listen(seek, 'lostpointercapture', () => { if (dragging) { release(); hidePreview(); sync(); pointerCommit = seek.value; } });
      // Native range keyboard behavior supplies arrows, Page Up/Down, Home/End.
      listen(seek, 'keydown', () => { pointerCommit = null; });
      listen(seek, 'change', () => { if (!dragging && seek.value !== pointerCommit) commit(seek.value); });
      listen(seek, 'focus', () => showPreview(Number(seek.value)));
      listen(seek, 'blur', () => { if (!dragging) hidePreview(); });
      listen(seek, 'pointerleave', () => { if (!dragging && document.activeElement !== seek) hidePreview(); });
      const syncCaptions = () => {
        if (disposed) return;
        const tracks = Array.from(video.textTracks).filter(track => ['captions', 'subtitles'].includes(track.kind));
        const selected = tracks.findIndex(track => track.mode === 'showing');
        captions.replaceChildren(new Option(labels.off, '-1'));
        tracks.forEach((track, index) => captions.add(new Option(track.label || track.language || labels.captions, String(index))));
        captions.value = String(selected); captionField.hidden = tracks.length === 0;
      };
      listen(captions, 'change', () => {
        const selected = Number(captions.value);
        Array.from(video.textTracks).filter(track => ['captions', 'subtitles'].includes(track.kind))
          .forEach((track, index) => { track.mode = index === selected ? 'showing' : 'disabled'; });
        video.dispatchEvent(new Event('plamenu:captions-choice'));
      });
      ['addtrack', 'removetrack', 'change'].forEach(kind => listen(video.textTracks, kind, syncCaptions));
      fullscreen.hidden = !wrapper.requestFullscreen && !video.webkitEnterFullscreen;
      listen(fullscreen, 'click', async () => {
        try { if (document.fullscreenElement === wrapper) await document.exitFullscreen();
          else if (wrapper.requestFullscreen) await wrapper.requestFullscreen();
          else { video.controls = true; video.webkitEnterFullscreen(); } }
        catch { video.controls = false; }
      });
      listen(video, 'webkitendfullscreen', () => { video.controls = false; sync(); });
      listen(document, 'fullscreenchange', () => { hidePreview(); sync(); placeSettings(); showChrome(); });
      ['webkitbeginfullscreen', 'enterpictureinpicture', 'leavepictureinpicture', 'webkitpresentationmodechanged'].forEach(kind => listen(video, kind, sync));
      ['play', 'pause', 'playing', 'ended', 'timeupdate', 'volumechange', 'ratechange', 'seeked'].forEach(kind => listen(video, kind, sync));
      const syncMetadata = () => {
        if (isLive) { syncCaptions(); sync(); return; }
        if (!Number.isFinite(video.duration)) return;
        // WebKit can expose zero while completing an MP4 range read. Keep
        // the known storyboard duration until the finite media duration lands.
        if (video.duration <= 0) return;
        duration = video.duration; seek.max = String(duration);
        // Engine fallback listeners can restore the previous position in this
        // same metadata event. Apply the explicit user intent after them.
        if (pending !== null) queueMicrotask(() => {
          if (disposed || pending === null) return;
          video.currentTime = clamp(pending); pending = null; sync();
        });
        syncCaptions(); sync();
      };
      listen(video, 'loadedmetadata', syncMetadata);
      listen(video, 'emptied', () => { release(); hidePreview(); sync(); });
      listen(video, 'durationchange', () => {
        if (video.readyState < HTMLMediaElement.HAVE_METADATA) return;
        release(); hidePreview(); syncMetadata();
      });
      // A media error can still trigger the existing engine fallback. Return
      // native chrome immediately rather than trapping it behind broken UI.
      listen(video, 'error', () => dispose());
      const previousTabIndex = video.getAttribute('tabindex');
      const dispose = () => {
        if (disposed) return;
        disposed = true; release(); clearTimeout(chromeTimer); owner.abort();
        if (!video.isConnected) { video.pause(); hlsCleanup.get(video)?.(); }
        if (spriteUrl) URL.revokeObjectURL(spriteUrl);
        video.controls = true; gallery.hidden = !gallery.matches('details');
        if (previousTabIndex === null) video.removeAttribute('tabindex'); else video.setAttribute('tabindex', previousTabIndex);
        const quality = options.querySelector('.video-quality');
        if (quality && quality.dataset.qualityPending !== 'true') figure.append(quality);
        if (standard) { standard.classList.remove('vod-controls__standard'); standardSlot.replaceWith(standard); }
        if (goLive) liveSlot.replaceWith(goLive);
        if (wrapper.parentNode) { wrapper.before(video); wrapper.remove(); }
        vodControllers.delete(video);
      };
      // Bind every handler before removing the working native controls.
      video.before(wrapper); wrapper.append(video, controls);
      video.tabIndex = 0; syncCaptions(); sync(); gallery.hidden = true; video.controls = false;
      vodControllers.set(video, dispose);
    });
    if (!vodObserver && vodControllers.size) {
      vodObserver = new MutationObserver(records => {
        // Time labels also mutate the DOM. Scan controllers only when a
        // removed element contains a video, not on every timeupdate.
        if (!records.some(record => Array.from(record.removedNodes).some(node =>
          node.nodeType === Node.ELEMENT_NODE && (node.matches('video') || node.querySelector('video'))))) return;
        for (const [video, dispose] of vodControllers) if (!video.isConnected) dispose();
      });
      vodObserver.observe(document.body, {childList: true, subtree: true});
      window.addEventListener('pagehide', () => {
        for (const dispose of vodControllers.values()) dispose();
      });
      window.addEventListener('pageshow', () => bindVodControls(document));
    }
  }

  function bindVideoChapters(root) {
    root.querySelectorAll(".media__chapters a[data-chapter-seek]").forEach((link) => {
      if (link.dataset.chapterBound) return;
      link.dataset.chapterBound = "1";
      link.addEventListener("click", (event) => {
        const video = link.closest("figure")?.querySelector("video");
        const seconds = Number(link.dataset.chapterSeek);
        if (!video || !Number.isFinite(seconds) || seconds < 0) return;
        event.preventDefault();
        const seek = () => {
          try {
            video.currentTime = seconds;
            video.focus();
          } catch { /* Metadata not ready yet; the load event retries below. */ }
        };
        if (video.readyState >= HTMLMediaElement.HAVE_METADATA) {
          seek();
        } else {
          video.addEventListener("loadedmetadata", seek, { once: true });
          if (video.dataset.hls) video.play().catch(() => {});
          else video.load();
        }
      });
    });
  }

  // Opt-in history only exists on authenticated focus pages. No page view,
  // metadata preload, embed, live stream or anonymous player records history.
  function bindVideoHistory(root) {
    root.querySelectorAll('form[data-video-history]').forEach((form) => {
      if (form.dataset.historyBound) return;
      const video = form.closest('figure')?.querySelector('video');
      if (!video) return;
      form.dataset.historyBound = '1';
      const input = form.elements.position_seconds;
      const note = form.querySelector('[data-history-status]');
      let played = false;
      let busy = false;
      let pending = null;
      let lastSent = null;
      let lastSave = 0;
      const save = async (manual = false, snapshot = null) => {
        if (!manual && !played) return;
        const seconds = snapshot?.seconds ?? (manual ? Number(input.value) : Math.floor(video.currentTime));
        if (!Number.isFinite(seconds) || seconds < 0 || seconds > 604800) return;
        const completed = snapshot?.completed ?? (!manual && video.ended);
        if (busy) {
          // One bounded latest-position queue. An ending/pause or explicit
          // manual save must not disappear behind a slow periodic request.
          // Automatic updates cannot overwrite a queued manual correction.
          if (manual || !pending?.manual) pending = {seconds, completed, manual};
          return;
        }
        const value = `${seconds}:${completed}`;
        if (!manual && value === lastSent) return;
        busy = true;
        if (manual || document.activeElement !== input) input.value = String(seconds);
        const body = new URLSearchParams({ csrf: form.elements.csrf.value,
          position_seconds: String(seconds), completed: completed ? '1' : '0',
          content_revision: form.elements.content_revision?.value || '0' });
        try {
          const response = await fetch(form.action, { method: 'POST', body,
            credentials: 'same-origin', headers: { Accept: 'application/json' }, keepalive: true });
          if (!response.ok) throw new Error('History rejected');
          const result = await response.json();
          if (!result.saved) throw new Error('History disabled');
          lastSent = value;
          lastSave = Date.now();
          if (manual) note.textContent = form.dataset.historySaved;
        } catch { if (manual) note.textContent = form.dataset.historyFailed; }
        finally {
          busy = false;
          const queued = pending;
          pending = null;
          if (queued) save(queued.manual, queued);
        }
      };
      form.addEventListener('submit', (event) => {
        event.preventDefault();
        save(true);
      });
      video.addEventListener('playing', () => { played = true; });
      video.addEventListener('timeupdate', () => {
        if (played && !video.paused && Date.now() - lastSave > 15000) {
          lastSave = Date.now();
          save();
        }
      });
      video.addEventListener('pause', () => save());
      video.addEventListener('ended', () => save());
      window.addEventListener('pagehide', () => save());
      const resume = form.closest('figure').querySelector('[data-history-resume]');
      resume?.addEventListener('click', (event) => {
        const seconds = Number(resume.dataset.historyResume);
        if (!Number.isFinite(seconds)) return;
        event.preventDefault();
        const seek = () => { video.currentTime = seconds; video.focus(); };
        if (video.readyState >= HTMLMediaElement.HAVE_METADATA) seek();
        else {
          video.addEventListener('loadedmetadata', seek, { once: true });
          if (video.dataset.hls) video.play().catch(() => {});
          else video.load();
        }
      });
    });
  }

  function bindCaptionTranscript(root) {
    root.querySelectorAll(".caption-transcript a[data-transcript-seek-ms]").forEach((link) => {
      if (link.dataset.transcriptBound) return;
      link.dataset.transcriptBound = "1";
      link.addEventListener("click", (event) => {
        const video = link.closest(".caption-transcript")?.querySelector("video");
        const milliseconds = Number(link.dataset.transcriptSeekMs);
        if (!video || !Number.isFinite(milliseconds) || milliseconds < 0) return;
        event.preventDefault();
        const seek = () => {
          try {
            video.currentTime = milliseconds / 1000;
            video.focus();
          } catch { /* Metadata not ready yet; loadedmetadata retries. */ }
        };
        if (video.readyState >= HTMLMediaElement.HAVE_METADATA) seek();
        else {
          video.addEventListener("loadedmetadata", seek, { once: true });
          video.load();
        }
      });
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

  function updateInboxBadge(link, unread) {
    const bell = link.querySelector('.bell');
    if (!bell) return;
    if (unread && !bell.querySelector('.bell__dot')) {
      const dot = document.createElement('span'); dot.className = 'bell__dot'; bell.append(dot);
    } else if (!unread) bell.querySelector('.bell__dot')?.remove();
    const label = unread ? link.dataset.inboxUnread : link.dataset.inboxLabel;
    if (label) { link.setAttribute('aria-label', label); if (link.hasAttribute('title')) link.title = label; }
  }

  function bindInboxState() {
    if (!document.querySelector('[data-notification-nav]')) return;
    let refreshing = false;
    const refresh = async () => {
      if (refreshing || document.hidden) return;
      refreshing = true;
      try {
        const response = await fetch('/web/inbox/state', {credentials: 'same-origin', cache: 'no-store'});
        if (!response.ok || response.redirected) return;
        const state = await response.json();
        for (const [selector, unread] of [['[data-notification-nav]', state.notifications], ['[data-message-nav]', state.messages]]) {
          document.querySelectorAll(selector).forEach(link => {
            updateInboxBadge(link, unread);
          });
        }
        // Keep the list's read marker and its menu consistent with the bell
        // when another tab opens a conversation. Reuse the authorized HTML
        // representation; do not infer individual read state from a total.
        const conversations=document.querySelector('.conversations');
        if(conversations && location.pathname==='/conversations') {
          const page=await fetch(location.href,{credentials:'same-origin',cache:'no-store'});
          if(page.ok && !page.redirected) {
            const parsed=new DOMParser().parseFromString(await page.text(),'text/html');
            const fresh=new Map([...parsed.querySelectorAll('[data-conversation-id]')].map(row=>[row.dataset.conversationId,row]));
            conversations.querySelectorAll('[data-conversation-id]').forEach(row=>{
              const next=fresh.get(row.dataset.conversationId);
              if(next && row.classList.contains('is-unread')!==next.classList.contains('is-unread') && !row.contains(document.activeElement)) {
                row.replaceWith(next); enhance(next);
              }
            });
          }
        }
      } catch { /* A normal navigation also refreshes the unread state. */ }
      finally { refreshing = false; }
    };
    window.addEventListener('pageshow', refresh);
    document.addEventListener('plamenu:inbox-refresh', refresh);
    document.addEventListener('visibilitychange', refresh);
    if ('BroadcastChannel' in window) {
      const channel = new BroadcastChannel('plamenu-inbox-state');
      channel.addEventListener('message', refresh);
      channel.postMessage('refresh');
      window.addEventListener('pagehide', () => channel.close(), {once: true});
    }
  }

  function showLiveNotification(note) {
    const direct = note?.status?.visibility === "direct";
    document.querySelectorAll(direct ? "[data-message-nav]" : "[data-notification-nav]").forEach(link => updateInboxBadge(link, true));
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
          if (message.event === "notification") {
            const note = typeof message.payload === "string" ? JSON.parse(message.payload) : message.payload;
            showLiveNotification(note);
          }
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

  // The app-library page is complete server-rendered HTML. On capable clients,
  // reveal local search and status filters so large catalogs can be narrowed
  // without another request or displacing the moderator's current position.
  function bindLibraryFilters(root) {
    root.querySelectorAll("[data-library-filter]").forEach((section) => {
      if (section.dataset.libraryFilterBound) return;
      section.dataset.libraryFilterBound = "1";
      const controls = section.querySelector("[data-library-controls]");
      const search = section.querySelector("[data-library-search]");
      const state = section.querySelector("[data-library-state-filter]");
      const output = section.querySelector("[data-library-visible-count]");
      const empty = section.querySelector(".webxdc-admin-filter-empty");
      const records = [...section.querySelectorAll("[data-library-record]")];
      if (!controls || !search || !state || !output) return;

      const apply = () => {
        const needle = search.value.trim().toLocaleLowerCase();
        const selectedState = state.value;
        let visible = 0;
        records.forEach((record) => {
          const matchesText = !needle || (record.dataset.libraryText || "")
            .toLocaleLowerCase().includes(needle);
          const matchesState = !selectedState || record.dataset.libraryState === selectedState;
          record.hidden = !(matchesText && matchesState);
          if (!record.hidden) visible += 1;
        });
        output.value = `${visible} ${visible === 1 ? "result" : "results"}`;
        if (empty) empty.hidden = visible !== 0 || records.length === 0;
      };

      controls.hidden = false;
      search.addEventListener("input", apply);
      state.addEventListener("change", apply);
      apply();
    });
  }

  // Session creation keeps the complete app choice usable as ordinary radio
  // controls. Enhancement adds quick filtering, switches the upload panel,
  // and carries library metadata into the editable session fields.
  function bindWebxdcCreate(root) {
    root.querySelectorAll("form[data-webxdc-create]").forEach((form) => {
      if (form.dataset.webxdcCreateBound) return;
      form.dataset.webxdcCreateBound = "1";
      const choices = [...form.querySelectorAll("[data-webxdc-app-choice], [data-webxdc-upload-choice]")];
      const radios = choices.map((choice) => choice.querySelector('input[name="version_id"]'))
        .filter(Boolean);
      const uploadChoice = form.querySelector("[data-webxdc-upload-choice]");
      const uploadPanel = form.querySelector("[data-webxdc-package-upload]");
      const appBrowser = form.querySelector("[data-webxdc-app-browser]");
      const appBrowserLabel = form.querySelector("[data-webxdc-app-browser-label]");
      const appBrowserMeta = form.querySelector("[data-webxdc-app-browser-meta]");
      const file = uploadPanel?.querySelector('input[type="file"]');
      const name = form.querySelector('input[name="name"]');
      const summary = form.querySelector('textarea[name="summary"]');
      const nameFromApp = form.querySelector("[data-webxdc-name-from-app]");
      const summaryFromApp = form.querySelector("[data-webxdc-summary-from-app]");
      const enhanced = form.querySelector("[data-webxdc-create-enhanced]");
      if (!uploadChoice || !uploadPanel || !file || !name || !summary || !nameFromApp || !summaryFromApp) return;
      if (enhanced) enhanced.value = "1";

      let autoName = radios.find((radio) => radio.checked)?.closest("label")?.dataset.appName || "";
      let autoSummary = radios.find((radio) => radio.checked)?.closest("label")?.dataset.appSummary || "";
      const uploadedName = () => {
        const filename = file.files?.[0]?.name || "";
        return filename.replace(/\.xdc$/i, "").replace(/[-_]+/g, " ").trim();
      };
      const applyChoice = () => {
        const radio = radios.find((candidate) => candidate.checked);
        const choice = radio?.closest("label");
        const uploading = choice === uploadChoice;
        uploadPanel.hidden = !uploading;
        file.required = uploading;
        let nextName = choice?.dataset.appName || "";
        const nextSummary = choice?.dataset.appSummary || "";
        if (uploading) nextName = uploadedName();
        if (nameFromApp.value === "1" || !name.value.trim() || name.value === autoName) {
          name.value = nextName;
          nameFromApp.value = "1";
        }
        if (summaryFromApp.value === "1" || !summary.value.trim() || summary.value === autoSummary) {
          summary.value = nextSummary;
          summaryFromApp.value = "1";
        }
        autoName = nextName;
        autoSummary = nextSummary;
        if (appBrowserLabel && appBrowserMeta) {
          appBrowserLabel.textContent = uploading ? "Choose from app library" : nextName;
          appBrowserMeta.textContent = uploading
            ? appBrowserMeta.dataset.default
            : "Selected from app library";
        }
        if (!uploading && appBrowser) appBrowser.open = false;
      };
      radios.forEach((radio) => radio.addEventListener("change", applyChoice));
      file.addEventListener("change", applyChoice);
      name.addEventListener("input", () => { nameFromApp.value = "0"; });
      summary.addEventListener("input", () => { summaryFromApp.value = "0"; });
      applyChoice();

      const searchWrap = form.querySelector("[data-webxdc-create-search-wrap]");
      const search = form.querySelector("[data-webxdc-create-search]");
      const noResults = form.querySelector("[data-webxdc-create-no-results]");
      const savedChoices = choices.filter((choice) => choice.hasAttribute("data-webxdc-app-choice"));
      if (searchWrap && search && noResults) {
        searchWrap.hidden = false;
        const filter = () => {
          const needle = search.value.trim().toLocaleLowerCase();
          let visible = 0;
          savedChoices.forEach((choice) => {
            const matches = !needle || (choice.dataset.appSearch || "")
              .toLocaleLowerCase().includes(needle);
            choice.hidden = !matches;
            if (matches) visible += 1;
          });
          noResults.hidden = visible !== 0;
        };
        search.addEventListener("input", filter);
        filter();
      }
    });
  }

  // ---- Native-video upload and private draft progress --------------------
  // Only the first and last 64 KiB are hashed in JS; the File stays in the
  // browser's Blob store. Chunks are bounded at 8 MiB and server-acknowledged
  // offsets survive reloads. The unchanged multipart form is the no-JS path.
  async function videoFingerprint(file) {
    const size = Math.min(65536, file.size);
    const first = new Uint8Array(await file.slice(0, size).arrayBuffer());
    const last = new Uint8Array(await file.slice(file.size - size).arrayBuffer());
    const probe = new Uint8Array(first.length + last.length);
    probe.set(first);
    probe.set(last, first.length);
    const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", probe));
    return Array.from(hash, (byte) => byte.toString(16).padStart(2, "0")).join("");
  }

  async function videoJson(url, options) {
    const response = await fetch(url, { credentials: "same-origin", cache: "no-store", ...options });
    const value = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(value.error || `Upload request failed (${response.status})`);
    return value;
  }

  async function sendVideoChunks(id, file, fingerprint, csrf, panel, label, meter, mismatch) {
    const source = `/web/videos/drafts/${id}/source`;
    const status = await videoJson(source);
    if (status.total !== file.size || status.fingerprint !== fingerprint) {
      throw new Error(mismatch);
    }
    let offset = status.offset;
    let announced = -1;
    const progress = () => {
      const percent = Math.floor(100 * offset / file.size);
      meter.value = percent;
      const milestone = Math.floor(percent / 10) * 10;
      if (milestone > announced) {
        announced = milestone;
        label.textContent = `${panel.dataset.uploading} ${milestone}%`;
      }
    };
    progress();
    let failures = 0;
    while (offset < file.size) {
      const end = Math.min(file.size, offset + 8 * 1024 * 1024);
      try {
        const result = await videoJson(source, {
          method: "PATCH",
          headers: { "x-csrf-token": csrf, "x-upload-offset": String(offset) },
          body: file.slice(offset, end),
        });
        if (result.offset !== end) throw new Error("Upload offset was not acknowledged");
        offset = end;
        failures = 0;
        progress();
      } catch (error) {
        // A lost response may follow a committed chunk. Read the durable
        // offset before retrying; never blindly send an overlapping chunk.
        if (++failures > 3) throw error;
        const current = await videoJson(source);
        if (current.total !== file.size || current.fingerprint !== fingerprint) throw new Error(mismatch);
        if (current.offset < offset || current.offset > end) throw error;
        offset = current.offset;
        progress();
      }
    }
    label.textContent = panel.dataset.saving;
    await videoJson(`${source}/finish`, { method: "POST", headers: { "x-csrf-token": csrf } });
    location.assign(`/videos/drafts/${id}`);
  }

  function bindVideoPosterPicker(root) {
    root.querySelectorAll("form[data-video-upload]").forEach((form) => {
      const picker = form.querySelector("[data-poster-picker]");
      const fileInput = form.querySelector('input[name="file"]');
      const timeInput = form.querySelector("[data-poster-time]");
      const preview = picker?.querySelector("[data-poster-source]");
      const choose = picker?.querySelector("[data-poster-use-current]");
      const status = picker?.querySelector("[data-poster-picker-status]");
      if (!picker || !fileInput || !timeInput || !preview || !choose || !status
        || picker.dataset.posterPickerBound) return;
      picker.dataset.posterPickerBound = "1";
      let objectUrl;
      const release = () => {
        preview.pause();
        preview.removeAttribute("src");
        preview.load();
        timeInput.max = "86400";
        if (objectUrl) URL.revokeObjectURL(objectUrl);
        objectUrl = undefined;
      };
      fileInput.addEventListener("change", () => {
        release();
        picker.hidden = true;
        const file = fileInput.files?.[0];
        if (!file || !file.type.startsWith("video/")) return;
        objectUrl = URL.createObjectURL(file);
        preview.src = objectUrl;
        picker.hidden = false;
      });
      preview.addEventListener("loadedmetadata", () => {
        if (!Number.isFinite(preview.duration) || preview.duration <= 0) return;
        timeInput.max = String(Math.min(86400, Math.max(0, preview.duration - 1)));
        preview.currentTime = Math.min(Number(timeInput.value) || 0, Number(timeInput.max));
      });
      timeInput.addEventListener("change", () => {
        if (!picker.hidden && Number.isFinite(preview.duration) && timeInput.validity.valid) {
          preview.currentTime = Math.min(Number(timeInput.value) || 0, Number(timeInput.max));
        }
      });
      choose.addEventListener("click", () => {
        if (!Number.isFinite(preview.currentTime)) return;
        preview.pause();
        const selected = Math.min(preview.currentTime, Number(timeInput.max));
        preview.currentTime = selected;
        timeInput.value = (Math.floor(selected * 10) / 10).toFixed(1);
        status.textContent = `${picker.dataset.posterStatusPrefix} ${timeInput.value} s`;
      });
      window.addEventListener("pagehide", release, { once: true });
    });
  }

  function bindVideoPosterReplacement(root) {
    root.querySelectorAll("form[data-video-poster-replace]").forEach((form) => {
      const fileInput = form.querySelector('input[name="poster_file"]');
      const preview = form.querySelector("[data-poster-replace-preview]");
      if (!fileInput || !preview || form.dataset.posterReplaceBound) return;
      form.dataset.posterReplaceBound = "1";
      let objectUrl;
      const release = () => {
        preview.hidden = true;
        preview.removeAttribute("src");
        if (objectUrl) URL.revokeObjectURL(objectUrl);
        objectUrl = undefined;
      };
      fileInput.addEventListener("change", () => {
        release();
        const file = fileInput.files?.[0];
        if (!file || !file.type.startsWith("image/")) return;
        objectUrl = URL.createObjectURL(file);
        preview.src = objectUrl;
        preview.hidden = false;
      });
      preview.addEventListener("error", release);
      window.addEventListener("pagehide", release, { once: true });
    });
  }

  function bindCaptionUpload(root) {
    root.querySelectorAll("form[data-caption-upload]").forEach((form) => {
      if (form.dataset.captionBound) return;
      form.dataset.captionBound = "1";
      const input = form.querySelector('input[name="caption"]');
      const status = form.querySelector("[data-caption-status]");
      if (!input || !status) return;
      input.addEventListener("change", async () => {
        const file = input.files?.[0];
        input.setCustomValidity("");
        status.hidden = !file;
        if (!file) return;
        let message;
        if (file.size > 256 * 1024) {
          message = form.dataset.captionTooLarge;
        } else {
          try {
            const content = (await file.text()).replace(/^\uFEFF/, "");
            const lines = content.split(/\r?\n/);
            const cues = lines.filter((line) => /\d.*-->.*\d/.test(line)).length;
            message = /^WEBVTT(?:[ \t]|$)/.test(lines[0]) && cues && !content.includes("\0")
              ? `${form.dataset.captionReady} ${cues}`
              : form.dataset.captionInvalid;
          } catch (_error) {
            message = form.dataset.captionInvalid;
          }
        }
        const valid = message?.startsWith(form.dataset.captionReady);
        if (!valid) input.setCustomValidity(message || form.dataset.captionInvalid);
        status.textContent = message;
      });
    });
  }

  function bindChapterEditors(root) {
    root.querySelectorAll("form[data-chapter-editor]").forEach((form) => {
      if (form.dataset.chapterBound) return;
      form.dataset.chapterBound = "1";
      const textarea = form.querySelector('textarea[name="chapters"]');
      const status = form.querySelector('[data-chapter-status]');
      const preview = form.querySelector('[data-chapter-preview]');
      const current = form.querySelector('[data-chapter-current]');
      const video = form.closest('section.column')?.querySelector('video.video-publish__preview');
      if (!textarea || !status || !preview) return;
      const read = () => {
        const chapters = [];
        const seen = new Set();
        for (const line of textarea.value.split(/\r?\n/)) {
          if (!line.trim()) continue;
          const match = line.trim().match(/^(\S+)\s+(.+)$/);
          if (!match) return null;
          const pieces = match[1].split(':');
          if (pieces.length < 2 || pieces.length > 3 || pieces.some(part => !/^\d+$/.test(part))) return null;
          const nums = pieces.map(Number);
          const seconds = nums.pop();
          const minutes = nums.pop();
          const hours = nums.pop() || 0;
          const start = hours * 3600 + minutes * 60 + seconds;
          const title = match[2].trim();
          if (seconds >= 60 || (pieces.length === 3 && minutes >= 60) ||
              start > 604800 || !title || [...title].length > 200 || seen.has(start)) return null;
          seen.add(start);
          chapters.push({ start, title, stamp: match[1] });
          if (chapters.length > 100) return null;
        }
        return chapters.sort((a, b) => a.start - b.start);
      };
      const refresh = () => {
        const chapters = read();
        textarea.setCustomValidity(chapters ? '' : form.dataset.chapterInvalid);
        status.hidden = false;
        status.textContent = chapters ? `${form.dataset.chapterReady} ${chapters.length}` : form.dataset.chapterInvalid;
        preview.replaceChildren();
        preview.hidden = !chapters?.length;
        chapters?.forEach(({ stamp, title }) => {
          const item = document.createElement('li');
          item.textContent = `${stamp} ${title}`;
          preview.append(item);
        });
      };
      textarea.addEventListener('input', refresh);
      if (video && current) {
        current.hidden = false;
        current.addEventListener('click', () => {
          const seconds = Math.floor(video.currentTime);
          if (!Number.isFinite(seconds) || seconds < 0) return;
          const stamp = seconds >= 3600
            ? `${Math.floor(seconds / 3600)}:${String(Math.floor(seconds / 60) % 60).padStart(2, '0')}:${String(seconds % 60).padStart(2, '0')}`
            : `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
          textarea.value += `${textarea.value.trim() ? '\n' : ''}${stamp} `;
          textarea.focus();
          textarea.setSelectionRange(textarea.value.length, textarea.value.length);
          refresh();
        });
      }
      refresh();
    });
  }

  function bindVideoUpload(root) {
    root.querySelectorAll("[data-video-licence]").forEach((select) => {
      if (select.dataset.licenceBound) return;
      select.dataset.licenceBound = "1";
      const status = select.closest("form")?.querySelector("[data-video-licence-status]");
      const link = status?.querySelector("[data-video-licence-link]");
      if (!status || !link) return;
      const update = () => {
        const option = select.selectedOptions[0];
        const url = option?.dataset.licenceUrl;
        status.hidden = !url;
        if (url) { link.href = url; link.textContent = option.textContent; }
      };
      select.addEventListener("change", update);
      update();
    });
    root.querySelectorAll("form[data-video-schedule-form]").forEach((form) => {
      const schedule = form.querySelector("[data-video-schedule-at]");
      const scheduleButton = form.querySelector("[data-video-schedule-submit]");
      if (schedule && scheduleButton && !form.dataset.videoScheduleBound) {
        form.dataset.videoScheduleBound = "1";
        scheduleButton.addEventListener("click", () => { schedule.required = true; });
        form.querySelector('button[value="publish"]')?.addEventListener("click", () => {
          schedule.required = false;
        });
      }
    });
    root.querySelectorAll("form[data-video-upload]").forEach((form) => {
      if (form.dataset.videoUploadBound) return;
      form.dataset.videoUploadBound = "1";
      const panel = form.querySelector("[data-video-upload-progress]");
      const label = form.querySelector("[data-video-upload-label]");
      const meter = form.querySelector("[data-video-upload-meter]");
      const error = form.querySelector("[data-video-upload-error]");
      const submit = form.querySelector('button[type="submit"]');
      if (!panel || !label || !meter || !error || !submit || !globalThis.crypto?.subtle) return;
      form.addEventListener("submit", async (event) => {
        if (form.dataset.videoUploadPending) { event.preventDefault(); return; }
        const file = form.querySelector('input[name="file"]')?.files?.[0];
        if (!file) return;
        event.preventDefault();
        form.dataset.videoUploadPending = "1";
        submit.disabled = true;
        panel.hidden = false;
        error.hidden = true;
        meter.value = 0;
        label.textContent = `${panel.dataset.uploading} 0%`;
        let id;
        try {
          const fingerprint = await videoFingerprint(file);
          const metadata = new FormData(form);
          metadata.delete("file");
          metadata.set("source_size", String(file.size));
          metadata.set("source_fingerprint", fingerprint);
          const created = await videoJson("/web/videos/resumable", { method: "POST", body: metadata });
          id = created.id;
          await sendVideoChunks(id, file, fingerprint, metadata.get("csrf"), panel, label, meter, panel.dataset.mismatch);
        } catch (reason) {
          panel.hidden = true;
          error.textContent = reason?.message || panel.dataset.failed;
          if (id) {
            const link = document.createElement("a");
            link.href = `/videos/drafts/${id}`;
            link.textContent = panel.dataset.resumeLink;
            error.append(" ", link);
          } else {
            form.dataset.videoUploadPending = "";
            submit.disabled = false;
          }
          error.hidden = false;
          error.focus();
        }
      });
    });
  }

  function bindVideoResume(root) {
    root.querySelectorAll("form[data-video-resume]").forEach((form) => {
      if (form.dataset.videoResumeBound || !globalThis.crypto?.subtle) return;
      form.dataset.videoResumeBound = "1";
      form.hidden = false;
      const draft = form.closest("[data-video-draft]");
      const panel = form.querySelector("[data-video-upload-progress]");
      const label = form.querySelector("[data-video-upload-label]");
      const meter = form.querySelector("[data-video-upload-meter]");
      const error = form.querySelector("[data-video-upload-error]");
      const submit = form.querySelector('button[type="submit"]');
      form.addEventListener("submit", async (event) => {
        event.preventDefault();
        const file = form.querySelector('input[name="file"]')?.files?.[0];
        if (!file || submit.disabled) return;
        submit.disabled = true;
        panel.hidden = false;
        error.hidden = true;
        try {
          const fingerprint = await videoFingerprint(file);
          await sendVideoChunks(draft.dataset.videoDraft, file, fingerprint,
            form.querySelector('input[name="csrf"]').value,
            panel, label, meter, form.dataset.mismatch);
        } catch (reason) {
          panel.hidden = true;
          submit.disabled = false;
          error.textContent = reason?.message || form.dataset.failed;
          error.hidden = false;
          error.focus();
        }
      });
    });
  }

  // The document's Refresh link remains the entire no-JS experience. Poll
  // only while this page is visible, never mutate the draft via GET, and
  // announce a phase once rather than repeating every response to screen
  // readers. The server supplies locale-specific phase text in HTML.
  function bindVideoDraft(root) {
    root.querySelectorAll("[data-video-draft]").forEach((draft) => {
      if (draft.dataset.videoDraftBound) return;
      draft.dataset.videoDraftBound = "1";
      const labels = {
        receiving: draft.dataset.phaseReceiving,
        importing: draft.dataset.phaseImporting,
        "import-failed": draft.dataset.phaseImportFailed,
        queued: draft.dataset.phaseQueued,
        processing: draft.dataset.phaseProcessing,
        ready: draft.dataset.phaseReady,
        failed: draft.dataset.phaseFailed,
        expired: draft.dataset.phaseExpired,
        unavailable: draft.dataset.phaseUnavailable,
      };
      const terminal = new Set(["ready", "failed", "import-failed", "expired", "unavailable"]);
      if (terminal.has(draft.dataset.phase)) return;
      const id = draft.dataset.videoDraft;
      const controller = new AbortController();
      let timer;
      let interval = 2000;
      const stop = () => {
        controller.abort();
        clearTimeout(timer);
      };
      const poll = async () => {
        if (controller.signal.aborted || !draft.isConnected) return;
        if (document.hidden) {
          timer = setTimeout(poll, 2000);
          return;
        }
        try {
          const response = await fetch(`/videos/drafts/${id}/status`, {
            credentials: "same-origin",
            cache: "no-store",
            signal: controller.signal,
          });
          if (response.status === 404 || response.status === 401) { stop(); return; }
          if (!response.ok) throw new Error("Draft status unavailable");
          const { phase } = await response.json();
          if (labels[phase] && phase !== draft.dataset.phase) {
            draft.dataset.phase = phase;
            draft.querySelector('[role="status"]').textContent = labels[phase];
            draft.querySelector("[data-video-review]").hidden = phase !== "ready" || draft.hasAttribute("data-video-scheduled");
            draft.querySelector("[data-video-retry]").hidden = !["failed", "import-failed"].includes(phase) || draft.hasAttribute("data-video-scheduled");
          }
          if (terminal.has(phase)) { stop(); return; }
        } catch (_error) {
          if (controller.signal.aborted) return;
        }
        interval = Math.min(15000, Math.round(interval * 1.25));
        timer = setTimeout(poll, interval);
      };
      window.addEventListener("pagehide", stop, { once: true });
      timer = setTimeout(poll, interval);
    });
  }

  function bindVideoEmbedCodes(root) {
    root.querySelectorAll('[data-video-embed-code]').forEach(panel => {
      if (panel.dataset.copyBound) return;
      panel.dataset.copyBound = '1';
      const code = panel.querySelector('textarea');
      const button = panel.querySelector('[data-video-embed-copy]');
      const result = panel.querySelector('[data-video-embed-result]');
      button.hidden = false;
      button.addEventListener('click', async () => {
        if (button.disabled) return;
        button.disabled = true;
        let timer;
        try {
          await Promise.race([
            navigator.clipboard.writeText(code.value),
            new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Copy timed out')), 10000); })
          ]);
          result.textContent = panel.dataset.copied;
        } catch {
          result.textContent = panel.dataset.copyFailed;
          code.focus();
          code.select();
        } finally {
          clearTimeout(timer);
          button.disabled = false;
          result.hidden = false;
        }
      });
    });
  }

  function updateVideoEmbedChoices() {
    document.querySelectorAll('[data-video-settings-panel]').forEach(panel => {
      const choice = panel.querySelector('.media__p2p-toggle');
      const code = panel.querySelector('[data-video-embed-code] textarea');
      if (choice && code) code.value = code.value.replace(/p2p=[01]/, `p2p=${choice.checked ? 1 : 0}`);
    });
  }

  let videoSettingsDialog = null;
  function bindVideoSettings(root) {
    root.querySelectorAll('[data-video-settings-open]').forEach(link => {
      if (link.dataset.settingsBound) return;
      link.dataset.settingsBound = '1';
      if (window.HTMLDialogElement?.prototype.showModal) link.setAttribute('aria-haspopup', 'dialog');
      link.addEventListener('click', event => {
        // Modified clicks and browsers without native dialogs retain the link.
        if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey || event.button) return;
        if (!window.HTMLDialogElement?.prototype.showModal) return;
        const owner = link.closest('.status') || link.closest('.status-embed');
        const panels = Array.from(owner?.querySelectorAll('[data-video-settings-panel]') || [])
          .filter(panel => panel.closest('[data-video-owner]')?.dataset.videoOwner === link.dataset.videoSettingsStatus);
        if (!panels.length) return;
        event.preventDefault();
        videoSettingsDialog?.close();
        const dialog = document.createElement('dialog');
        dialog.className = 'video-settings-dialog';
        dialog.setAttribute('aria-label', link.dataset.videoSettingsTitle);
        const header = document.createElement('header');
        const title = document.createElement('h2');
        title.textContent = link.dataset.videoSettingsTitle;
        const close = document.createElement('button');
        close.type = 'button';
        close.className = 'video-settings-dialog__close';
        close.setAttribute('aria-label', link.dataset.videoSettingsClose);
        close.textContent = '×';
        close.addEventListener('click', () => dialog.close());
        header.append(title, close);
        dialog.append(header);
        const menu = link.closest('details');
        const restoreFocus = menu?.querySelector('summary') || link;
        const origins = panels.map(panel => {
          const marker = document.createComment('video settings');
          panel.before(marker);
          panel.hidden = false;
          dialog.append(panel);
          return {panel, marker};
        });
        if (menu) menu.open = false;
        dialog.addEventListener('close', () => {
          origins.forEach(({panel, marker}) => {
            panel.hidden = true;
            if (marker.parentNode) marker.replaceWith(panel);
          });
          dialog.remove();
          if (videoSettingsDialog === dialog) videoSettingsDialog = null;
          if (restoreFocus.isConnected) restoreFocus.focus();
        }, {once:true});
        dialog.addEventListener('click', e => {
          const box = dialog.getBoundingClientRect();
          if (e.target === dialog && (e.clientX < box.left || e.clientX > box.right || e.clientY < box.top || e.clientY > box.bottom)) dialog.close();
        });
        dialog.addEventListener('keydown', e => {
          if (e.key !== 'Tab') return;
          // Native inertness protects the page, but Chromium can move from
          // the first control into browser chrome on Shift+Tab. Keep both
          // keyboard boundaries inside this modal, using visible controls.
          const controls = Array.from(dialog.querySelectorAll('a[href],button:not(:disabled),input:not(:disabled),select:not(:disabled),textarea:not(:disabled),[tabindex="0"]'))
            .filter(control => control.getClientRects().length);
          const first = controls[0], last = controls.at(-1);
          if (e.shiftKey && document.activeElement === first) {
            e.preventDefault(); last.focus();
          } else if (!e.shiftKey && document.activeElement === last) {
            e.preventDefault(); first.focus();
          }
        });
        document.body.append(dialog);
        videoSettingsDialog = dialog;
        updateVideoEmbedChoices();
        dialog.showModal();
        close.focus();
      });
    });
  }

  function bindVideoSources(root) {
    root.querySelectorAll('[data-video-source-sync][data-source-pending="true"]').forEach(panel => {
      if (panel.dataset.sourceBound) return;
      panel.dataset.sourceBound = "1";
      const controller = new AbortController();
      const deadline = Date.now() + 10 * 60 * 1000;
      let timer, failures = 0;
      const stop = () => { controller.abort(); clearTimeout(timer); };
      const poll = async () => {
        if (controller.signal.aborted || !panel.isConnected || Date.now() >= deadline) { stop(); return; }
        if (document.hidden) { timer = setTimeout(poll, 5000); return; }
        try {
          const response = await fetch('/videos/sources/state', {credentials:'same-origin',cache:'no-store',signal:AbortSignal.any([controller.signal,AbortSignal.timeout(10000)])});
          if (!response.ok) throw new Error('Source status unavailable');
          const sources = await response.json();
          if (!Array.isArray(sources) || sources.length > 5) throw new Error('Invalid source status');
          for (const source of sources) {
            const label = [...panel.querySelectorAll('[data-source-state]')].find(node => node.dataset.sourceState === source.id);
            if (label && typeof source.label === 'string' && source.label.length < 1000 && label.textContent.trim() !== source.label) label.textContent = source.label;
          }
          failures = 0;
          panel.querySelector('[data-source-refresh-error]').hidden = true;
          if (!sources.some(source => source.checking === true)) { stop(); return; }
        } catch {
          if (controller.signal.aborted) return;
          const error = panel.querySelector('[data-source-refresh-error]');
          if (error.hidden) { error.textContent = panel.dataset.sourceRefreshFailed; error.hidden = false; }
          if (++failures >= 6) { stop(); return; }
        }
        timer = setTimeout(poll, Math.min(30000, 3000 * 2 ** failures));
      };
      addEventListener('pagehide', stop, {once:true});
      timer = setTimeout(poll, 1000);
    });
  }

  function bindLiveSchedules(root) {
    root.querySelectorAll("form[data-live-schedules]").forEach(form => {
      if (form.dataset.liveScheduleBound) return;
      form.dataset.liveScheduleBound = "true";
      const helper = form.querySelector("[data-live-schedule-picker]");
      const picker = form.querySelector("[data-live-schedule-date]");
      const primary = form.querySelector('[name="scheduled_at"]');
      const additional = form.querySelector('[name="scheduled_starts"]');
      const status = form.querySelector("[data-live-schedule-status]");
      if (!helper || !picker || !primary || !additional || !status) return;
      helper.hidden = false;
      form.querySelector("[data-live-schedule-add]").addEventListener("click", () => {
        const value = picker.value;
        if (!value) { picker.focus(); return; }
        const dates = additional.value.split(/\r?\n/).map(line => line.trim()).filter(Boolean);
        if (primary.value === value || dates.includes(value)) {
          status.textContent = form.dataset.duplicate;
          picker.focus();
          return;
        }
        dates.push(value);
        dates.sort();
        additional.value = dates.join("\n");
        picker.value = "";
        status.textContent = form.dataset.added;
        picker.focus();
      });
    });
  }

  // Playlist clips reuse the normal HLS/progressive player. No autoplay or
  // forced navigation: finish is announced, and Next remains a native link.
  function bindPlaylistClips(root) {
    root.querySelectorAll("[data-playlist-start]").forEach(region => {
      const video = region.querySelector("video");
      if (!video || region.dataset.clipBound) return;
      region.dataset.clipBound = "1";
      const start = Number(region.dataset.playlistStart);
      const stop = region.dataset.playlistStop === "" ? null : Number(region.dataset.playlistStop);
      if (!Number.isFinite(start) || start < 0 || (stop !== null && (!Number.isFinite(stop) || stop <= start))) return;
      let started = false;
      let finished = false;
      const status = region.querySelector("[data-playlist-status]");
      const seekStart = () => {
        if (started || video.readyState < HTMLMediaElement.HAVE_METADATA) return;
        try { video.currentTime = start; started = true; } catch { /* retry when metadata is ready */ }
      };
      // The normal player replaces progressive src with HLS on first play.
      // Its load() clears the earlier seek, so apply the clip to the new source
      // too, without resetting it for an ordinary pause/resume.
      video.addEventListener("emptied", () => { started = false; finished = false; });
      video.addEventListener("loadedmetadata", seekStart);
      video.addEventListener("play", () => {
        if (finished) { finished = false; started = false; }
        seekStart();
        if (status) status.textContent = "";
      });
      video.addEventListener("timeupdate", () => {
        if (stop === null || video.currentTime < stop || finished) return;
        video.pause(); finished = true;
        if (status) status.textContent = region.dataset.playlistEnded;
      });
      seekStart();
    });
  }

  function bindPlaylistMoves(root) {
    root.querySelectorAll("form[data-playlist-move]").forEach(form => {
      if (form.dataset.moveBound) return;
      form.dataset.moveBound = "1";
      const position = form.elements.namedItem("position");
      form.querySelectorAll("[data-playlist-delta]").forEach(button => {
        button.hidden = false;
        const target = Number(position.value) + Number(button.dataset.playlistDelta);
        button.disabled = target < Number(position.min) || target > Number(position.max);
        button.addEventListener("click", () => {
          position.value = String(target);
          try { sessionStorage.setItem("plamenu-playlist-focus", JSON.stringify({
            path: location.pathname, entry: form.elements.namedItem("entry_id").value,
          })); } catch { /* native submission remains available without storage */ }
          form.requestSubmit();
        });
      });
    });
    try {
      const saved = JSON.parse(sessionStorage.getItem("plamenu-playlist-focus") || "null");
      if (saved?.path === location.pathname && /^\d+$/.test(saved.entry)) {
        sessionStorage.removeItem("plamenu-playlist-focus");
        const entry = document.getElementById(`entry-${saved.entry}`);
        const details = entry?.querySelector("details");
        if (details) { details.open = true; details.querySelector('[name="position"]')?.focus(); }
      }
    } catch { /* storage is optional */ }
  }

  // Keep the actual player node alive while navigating the listening workspace.
  // Every intercepted link/form retains its normal server-rendered fallback.
  let musicNavigationPending = false;
  let musicDock;
  function dockMusicPlayer(player) {
    if (musicDock) return musicDock;
    musicDock = player;
    player.classList.add('music-dock');
    player.setAttribute('role','region');
    player.setAttribute('aria-label', player.querySelector('h1')?.textContent || 'Music player');
    const tools = document.createElement('div'); tools.className = 'music-dock__actions';
    for (const action of ['previous','next']) {
      const button = document.createElement('button'); button.type = 'button';
      const existing = player.querySelector(`form.music-player-command input[name=action][value=${action}]`)?.form.querySelector('button');
      button.textContent = existing?.textContent || action;
      button.className = 'button--quiet';
      button.addEventListener('click', () => player.querySelector('audio.music-player-audio').dispatchEvent(new CustomEvent('plamenu:music-command', {detail:action})));
      tools.append(button);
    }
    const queue = document.createElement('button'); queue.type = 'button'; queue.className = 'button--quiet';
    queue.dataset.musicQueueToggle = '';
    queue.textContent = player.dataset.queueLabel || 'Queue'; queue.setAttribute('aria-expanded','false');
    queue.addEventListener('click', () => setMusicQueue(player,!player.classList.contains('music-dock--expanded')));
    const details = player.querySelector('.music-player-detail');
    if(details) {details.classList.add('button--quiet'); tools.append(details);}
    tools.append(queue);
    const close = document.createElement('button'); close.type = 'button'; close.className = 'button--quiet';
    close.textContent = player.dataset.closeLabel || 'Close player';
    close.addEventListener('click', () => {
      // Keep the controller's references valid; this also preserves its queue.
      player.querySelector('audio.music-player-audio').pause();
      player.hidden = true; document.body.classList.remove('has-music-dock');
    });
    tools.append(close); player.querySelector('.music-player-media').append(tools);
    document.body.append(player); document.body.classList.add('has-music-dock');
    history.scrollRestoration = 'manual';
    let scrollTimer;
    window.addEventListener('scroll',()=>{clearTimeout(scrollTimer);scrollTimer=setTimeout(()=>history.replaceState({...history.state,scrollY:window.scrollY},'',location.href),100);},{passive:true});
    bindMusicPlayer();
    return player;
  }
  function setMusicQueue(player, open) {
    player.classList.toggle('music-dock--expanded',open);
    player.querySelector('[data-music-queue-toggle]').setAttribute('aria-expanded',String(open));
    const queue=player.querySelector('.music-player-queue'); if(queue)queue.open=open;
  }
  function restoreMusicDock() {
    if (!musicDock) return;
    musicDock.hidden = false; document.body.classList.add('has-music-dock');
  }
  function musicNavigationUrl(url) {
    if (url.origin !== location.origin || url.hash || /\/(stream|downloads?|artwork|export)(\/|$)/.test(url.pathname)) return false;
    return url.pathname === '/' || /^\/(music(?:\/|$)|@|public$|notifications(?:\/|$)|conversations$)/.test(url.pathname);
  }
  async function showMusicDocument(response, push = true) {
    const target = new URL(response.url);
    if (target.origin !== location.origin || target.pathname === '/login') { location.assign(target); return; }
    const parsed = new DOMParser().parseFromString(await response.text(), 'text/html');
    const incoming = parsed.querySelector('main.app-main');
    if (!response.ok || !incoming) throw new Error('Navigation unavailable');
    if (incoming.querySelector('.music-player-page') && musicDock) {
      restoreMusicDock(); setMusicQueue(musicDock,true);
      return;
    }
    const main = document.querySelector('main.app-main');
    if (!main) { location.assign(target); return; }
    main.querySelectorAll('audio,video').forEach(media => media.pause());
    main.replaceChildren(...incoming.childNodes);
    document.title = parsed.title;
    if (push) history.pushState({musicNavigation:true, scrollY:0}, '', target.pathname + target.search);
    enhance(main); watchPager(main);
    main.focus({preventScroll:true}); window.scrollTo(0,push ? 0 : history.state?.scrollY || 0);
    document.dispatchEvent(new Event('plamenu:inbox-refresh'));
    document.querySelectorAll('details[data-drawer]').forEach(drawer => drawer.open=false);
  }
  async function musicNavigate(url, options = {}, push = true) {
    if (musicNavigationPending) return;
    if (push) history.replaceState({...history.state, scrollY:window.scrollY}, '', location.href);
    musicNavigationPending = true;
    const main = document.querySelector('main.app-main'); main?.setAttribute('aria-busy','true');
    try {
      const response = await fetch(url, {credentials:'same-origin', cache:'no-store', ...options});
      await showMusicDocument(response,push);
    } catch {
      if (!options.method) location.assign(url);
      else {
        const error = musicDock?.querySelector('.music-player-error');
        if (error) error.textContent = error.dataset.error;
      }
    } finally { musicNavigationPending=false; main?.removeAttribute('aria-busy'); }
  }
  function bindMusicWorkspace() {
    document.addEventListener('submit', async event => {
      const form = event.target;
      if (event.defaultPrevented || !(form instanceof HTMLFormElement)) return;
      if (form.matches('.music-play-now')) {
        event.preventDefault();
        const button = event.submitter; if (button) button.disabled=true;
        try {
          const data = new URLSearchParams(new FormData(form));
          const response = await fetch(form.action,{method:'POST',body:data,credentials:'same-origin'});
          const parsed = new DOMParser().parseFromString(await response.text(),'text/html');
          const player = parsed.querySelector('.music-player-page');
          if (!response.ok || !player || !player.querySelector('.music-player')?.dataset.current) throw new Error('Playback unavailable');
          const existing = document.querySelector('.music-player-page:has(audio.music-player-audio)');
          if (!musicDock && existing) dockMusicPlayer(existing);
          if (musicDock) {
            restoreMusicDock();
            musicDock.querySelector('audio.music-player-audio').dispatchEvent(new Event('plamenu:music-refresh'));
          } else {
            dockMusicPlayer(player);
            player.querySelector('audio.music-player-audio').play().catch(()=>{});
          }
        } catch {
          let error=form.querySelector('[role=alert]');
          if (!error) {error=document.createElement('p');error.setAttribute('role','alert');form.append(error);}
          error.textContent=form.dataset.error || 'Could not play this track. Try again.';
        } finally { if(button) button.disabled=false; }
        return;
      }
      if (!musicDock || musicDock.contains(form)) return;
      const action = new URL(form.action);
      if (form.method.toLowerCase()==='get' && musicNavigationUrl(action)) {
        event.preventDefault(); action.search=new URLSearchParams(new FormData(form)).toString(); musicNavigate(action);
      } else if (action.origin===location.origin && action.pathname.startsWith('/web/music/') && ![...new FormData(form).values()].some(value=>value instanceof File && value.size)) {
        event.preventDefault(); const data = new URLSearchParams(new FormData(form));
        if(event.submitter?.name) data.set(event.submitter.name,event.submitter.value);
        musicNavigate(action,{method:'POST',body:data});
      }
    });
    document.addEventListener('click', event => {
      if(event.defaultPrevented || event.button || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      const link=event.target.closest('a[href]');
      if(!link || link.hasAttribute('download') || (link.target && link.target!=='_self')) return;
      const url=new URL(link.href);
      let player=musicDock || document.querySelector('.music-player-page:has(audio.music-player-audio)');
      if(!player || !musicNavigationUrl(url)) return;
      if(!musicDock) player=dockMusicPlayer(player);
      event.preventDefault();
      if(url.pathname==='/music/player' && !url.searchParams.size){restoreMusicDock();setMusicQueue(player,!player.classList.contains('music-dock--expanded'));return;}
      musicNavigate(url);
    });
    window.addEventListener('popstate', () => {
      // The viewer consumes its own same-page entry; it isn't workspace
      // navigation and must not replace the document or restore saved scroll.
      if (lightbox?.historyActive) return;
      if (musicDock && musicNavigationUrl(new URL(location.href))) musicNavigate(location.href, {}, false);
    });
  }

  // ---- Wiring -----------------------------------------------------------
  function enhance(root) {
    bindDiscovery(root);
    bindVideoPosterPicker(root);
    bindVideoPosterReplacement(root);
    bindCaptionUpload(root);
    bindChapterEditors(root);
    bindVideoUpload(root);
    bindVideoResume(root);
    bindVideoDraft(root);
    bindVideoSources(root);
    bindVideoEmbedCodes(root);
    bindVideoSettings(root);
    bindFileLimits(root);
    bindLibraryFilters(root);
    bindWebxdcCreate(root);
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
    bindApplicationBulkSelection(root);
    bindModerationForms(root);
    bindReportForms(root);
    bindAnnouncementDismiss(root);
    bindReactionPickers(root);
    bindReactionForms(root);
    bindReactionTooltips(root);
    bindLightboxes(root);
    bindBlurhashes(root);
    bindHlsVideos(root);
    bindVodControls(root);
    bindVideoChapters(root);
    bindVideoHistory(root);
    bindPlaylistClips(root);
    bindPlaylistMoves(root);
    bindPodcastEpisodes(root);
    bindCaptionTranscript(root);
    bindMediaWarmup(root);
    bindCompose(root);
    bindComposeCombo(root);
    bindLanguageControls(root);
    bindSettingsDeps(root);
    bindWebauthn(root);
    bindPush(root);
    bindRemoteHistory(root);
    bindLiveNotifications();
    bindLiveSchedules(root);
    refreshTimes(root);
  }

  document.addEventListener("DOMContentLoaded", () => {
    registerAppWorker();
    bindInboxState();
    bindMusicWorkspace();
    enhance(document);
    watchPager(document);
    setInterval(() => refreshTimes(document), 60000);
  });
// Native details provide the no-script path. Keep optional explanations local,
// dismissible and keyboard reachable; validation opens hidden form sections.
document.addEventListener("invalid", (event) => {
  let section = event.target.closest("details");
  while (section) { section.open = true; section = section.parentElement.closest("details"); }
}, true);
document.querySelectorAll("[data-video-help]").forEach((help) => {
  const summary = help.querySelector("summary");
  const body = help.querySelector(".video-help__body");
  help.addEventListener("toggle", () => {
    if (help.open) placePopup(summary, body);
  });
  help.addEventListener("pointerenter", (event) => {
    if (event.pointerType === "mouse" && !help.open) { help.open = true; help.dataset.hoverOpen = "true"; }
  });
  const pointerInside = (event, element) => {
    const rect = element.getBoundingClientRect();
    return event.clientX >= rect.left && event.clientX <= rect.right
      && event.clientY >= rect.top && event.clientY <= rect.bottom;
  };
  const dismissHover = () => {
    if (help.dataset.hoverOpen && !help.contains(document.activeElement)) {
      help.open = false; delete help.dataset.hoverOpen;
    }
  };
  help.addEventListener("pointerleave", (event) => {
    // Plain explanation text passes clicks to the field below it. Keep a
    // hover preview open while the pointer is visually inside that text box.
    if (!pointerInside(event, body)) dismissHover();
  });
  document.addEventListener("pointermove", (event) => {
    if (event.pointerType === "mouse" && help.dataset.hoverOpen
      && !pointerInside(event, summary) && !pointerInside(event, body)) dismissHover();
  });
  summary.addEventListener("click", (event) => {
    if (help.dataset.hoverOpen) { event.preventDefault(); delete help.dataset.hoverOpen; }
  });
});
document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  document.querySelectorAll("[data-video-help][open]").forEach((help) => {
    const focused = help.contains(document.activeElement);
    help.open = false; delete help.dataset.hoverOpen;
    if (focused) { event.preventDefault(); help.querySelector("summary").focus(); }
  });
});
document.addEventListener("click", (event) => {
  document.querySelectorAll("[data-video-help][open]").forEach((help) => {
    if (!help.contains(event.target)) { help.open = false; delete help.dataset.hoverOpen; }
  });
});

// The optional player lives in its own ordinary window. Browsing the main
// window never replaces its media element; queue forms still work without JS.
function musicDevice() {
  try {
    let device = localStorage.getItem("plamenu-music-device");
    if (!device || !/^[a-zA-Z0-9_-]{1,64}$/.test(device)) {
      device = crypto.randomUUID();
      localStorage.setItem("plamenu-music-device", device);
    }
    return device;
  } catch { return "web"; }
}
document.querySelectorAll("a.music-player-open").forEach(link => {
  const url = new URL(link.href, location.href);
  url.searchParams.set("device", musicDevice());
  link.href = url.href;
  link.addEventListener("click", event => {
    if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey || event.button) return;
    const player = window.open("", "plamenu-music-player", `popup,width=${Math.min(640, window.innerWidth)},height=${Math.min(800, window.innerHeight)}`);
    if (player) {
      event.preventDefault();
      try {
        if (player.location.origin === location.origin && player.document.querySelector(".music-player")) {
          player.postMessage({ type: "plamenu-music-source", url: link.href }, location.origin);
        } else player.location.href = link.href;
      } catch { player.location.href = link.href; }
      player.focus();
    }
  });
});
function bindMusicPlayer() {
const musicAudio = document.querySelector("audio.music-player-audio");
if (musicAudio && !musicAudio.dataset.playerBound) {
  musicAudio.dataset.playerBound = "true";
  const error = document.querySelector(".music-player-error");
  const now = document.querySelector(".music-player-now");
  const device = document.querySelector(".music-player").dataset.device;
  const playerUrl = new URL("/music/player", location.origin);
  playerUrl.searchParams.set("device", device);
  let current = document.querySelector(".music-player").dataset.current;
  let pending = Promise.resolve();
  let saving = false;
  let lastSaved = Date.now();
  let refreshing = false;
  let manualBusy = 0;
  function busyButtons() {
    document.querySelector(".music-player-page").setAttribute("aria-busy", String(manualBusy > 0));
    document.querySelectorAll("form.music-player-command button").forEach(button => {
      if (manualBusy > 0) {
        if (!button.dataset.playerDisabled) button.dataset.playerDisabled = button.disabled ? "yes" : "no";
        button.disabled = true;
      } else if (button.dataset.playerDisabled) {
        button.disabled = button.dataset.playerDisabled === "yes";
        delete button.dataset.playerDisabled;
      }
    });
  }
  function formKey(form) {
    return ["action", "entry_id", "direction", "asset_id", "status_id", "episode_id", "remote_id", "collection_id", "podcast_id", "offset", "oldest"].map(key => form.elements.namedItem(key)?.value || "").join(":");
  }
  function preserveDrafts(old, next) {
    let focus = null;
    for (const form of old.querySelectorAll("form.music-player-command")) {
      const replacement = [...next.querySelectorAll("form.music-player-command")].find(other => formKey(form) === formKey(other));
      if (!replacement) continue;
      if (form.dataset.playerDirty) {
        for (const field of form.querySelectorAll("input:not([type=hidden]), select, textarea")) {
          const target = replacement.elements.namedItem(field.name);
          if (!target) continue;
          target.value = field.value;
          if (field.type === "checkbox") target.checked = field.checked;
        }
        replacement.dataset.playerDirty = "1";
      }
      const active = document.activeElement;
      if (form.contains(active)) {
        focus = active.name ? replacement.elements.namedItem(active.name) : replacement.querySelector("button");
      }
    }
    return focus;
  }
  function reportError() { error.textContent = error.dataset.error; }
  function formData(action) {
    const form = document.querySelector("form.music-player-command");
    if (!form) return null;
    const data = new URLSearchParams();
    for (const key of ["csrf", "device", "revision"]) data.set(key, form.elements.namedItem(key).value);
    data.set("action", action);
    return data;
  }
  async function refresh(action, url = playerUrl) {
    const response = await fetch(url, { credentials: "same-origin", cache: "no-store" });
    if (!response.ok || response.redirected) throw new Error("Player unavailable");
    const parsed = new DOMParser().parseFromString(await response.text(), "text/html");
    const next = parsed.querySelector("section.music-player");
    if (!next) throw new Error("Player unavailable");
    const changed = current !== next.dataset.current;
    const wasPlaying = !musicAudio.paused;
    const previous = document.querySelector("section.music-player");
    for (const opened of previous.querySelectorAll("details[open]")) {
      const label = opened.querySelector("summary")?.textContent;
      const entry=opened.closest("[data-entry-id]")?.dataset.entryId;
      const replacement = [...next.querySelectorAll("details")].find(detail => detail.querySelector("summary")?.textContent === label && detail.closest("[data-entry-id]")?.dataset.entryId === entry);
      if (replacement) replacement.open = true;
    }
    const focus = preserveDrafts(previous, next);
    previous.replaceWith(next);
    busyButtons();
    if (focus) focus.focus({ preventScroll: true });
    now.textContent = next.dataset.title;
    if(next.dataset.url)now.href=next.dataset.url;else now.removeAttribute("href");
    const detail = document.querySelector(".music-player-detail");
    detail.hidden = !next.dataset.url;
    if (next.dataset.url) detail.href = next.dataset.url;
    else detail.removeAttribute("href");
    const cover = document.querySelector(".music-player-artwork");
    cover.hidden = !next.dataset.image;
    if (next.dataset.image) { cover.src = next.dataset.image; cover.alt = next.dataset.title; }
    else cover.removeAttribute("src");
    if ("mediaSession" in navigator && "MediaMetadata" in window) {
      navigator.mediaSession.metadata = next.dataset.current ? new MediaMetadata({ title: next.dataset.title }) : null;
    }
    if (changed) {
      current = next.dataset.current;
      musicAudio.pause();
      if (next.dataset.stream) musicAudio.src = next.dataset.stream;
      else musicAudio.removeAttribute("src");
      musicAudio.replaceChildren();
      musicAudio.load();
    }
    if (changed || ["seek", "select", "next", "previous"].includes(action)) {
      const position = Number(next.dataset.position);
      if (musicAudio.readyState >= 1) musicAudio.currentTime = position;
      else musicAudio.addEventListener("loadedmetadata", () => { musicAudio.currentTime = position; }, { once: true });
    }
    if (["select", "next", "previous"].includes(action)) {
      reportListen(listening, true);
      listening = null;
    }
    if (current && (wasPlaying || ["select", "next", "previous"].includes(action))) {
      musicAudio.play().then(startListening).catch(() => { /* Native play controls remain available. */ });
    }
    bindQueueForms();
    if (["add", "collection"].includes(action)) history.replaceState(null, "", playerUrl);
    error.textContent = "";
  }
  function submit(data, action) {
    const manual = action !== "save";
    if (manual) { manualBusy++; busyButtons(); }
    // Read the revision when the command runs, after preceding commands commit.
    pending = pending.catch(() => {}).then(async () => {
      const fields = formData(action);
      if (!fields) return;
      if (action === "save" && document.querySelector(".music-player").dataset.current !== data.get("entry_id")) return;
      data.set("revision", fields.get("revision"));
      const response = await fetch("/web/music/player", {
        method: "POST", body: data, credentials: "same-origin",
        headers: { Accept: "application/json" },
      });
      if (!response.ok || response.redirected) {
        musicAudio.pause();
        await refresh();
        throw new Error("Queue command rejected");
      }
      await refresh(action);
    }).catch(reportError).finally(() => {
      if (manual) { manualBusy--; busyButtons(); }
    });
    return pending;
  }
  function bindQueueForms() {
    document.querySelectorAll("form.music-player-command").forEach(form => {
      if (form.dataset.playerBound) return;
      form.dataset.playerBound = "1";
      form.addEventListener("input", () => { form.dataset.playerDirty = "1"; });
      form.addEventListener("change", () => { form.dataset.playerDirty = "1"; });
      form.addEventListener("submit", event => {
        event.preventDefault();
        const data = new URLSearchParams(new FormData(form));
        delete form.dataset.playerDirty;
        submit(data, data.get("action"));
      });
    });
  }
  function savePosition() {
    if (!current || saving || !Number.isFinite(musicAudio.currentTime)) return;
    const data = formData("seek");
    if (!data) return;
    data.set("seconds", String(musicAudio.currentTime));
    data.set("entry_id", current);
    saving = true;
    lastSaved = Date.now();
    submit(data, "save").finally(() => { saving = false; });
  }
  // Only real playback events start a history session. Metadata, byte ranges
  // and queue reads never report a listen. Credit time advances, not seeks.
  let listening = null;
  const historyFeedback = document.querySelector(".music-history-feedback");
  function listeningData() {
    const form = document.querySelector("form.music-player-command");
    if (!form) return null;
    return new URLSearchParams({ csrf: form.elements.namedItem("csrf").value });
  }
  function startListening() {
    if (!current) return;
    if (listening && listening.entry === current && !listening.finished) {
      listening.previous = musicAudio.currentTime;
      listening.wall = performance.now();
      return;
    }
    const previous = listening;
    if (previous) reportListen(previous, true);
    const data = listeningData();
    if (!data) return;
    const session = { entry: current, total: 0, previous: musicAudio.currentTime,
      wall: performance.now(), lastReport: Date.now(), counted: false, finished: false,
      reporting: Promise.resolve() };
    data.set("request_key", crypto.randomUUID());
    data.set("queue_device", device);
    data.set("queue_entry_id", current);
    session.start = fetch("/web/music/listening/start", {
      method: "POST", body: data, credentials: "same-origin", headers: { Accept: "application/json" },
    }).then(async response => {
      if (!response.ok || response.redirected) return null;
      const result = await response.json();
      return result.enabled ? result.token : null;
    }).catch(() => null);
    listening = session;
    if (historyFeedback) historyFeedback.textContent = "";
  }
  function advanceListen() {
    const session = listening;
    if (!session || session.finished || session.entry !== current) return;
    const now = performance.now();
    const delta = musicAudio.currentTime - session.previous;
    const wall = Math.max(0, (now - session.wall) / 1000);
    if (!musicAudio.seeking && delta > 0 && delta < 5) session.total += Math.min(delta, wall);
    session.previous = musicAudio.currentTime;
    session.wall = now;
  }
  function reportListen(session, finish = false) {
    if (!session || session.finished || (!finish && session.counted)) return;
    if (finish) session.finished = true;
    session.lastReport = Date.now();
    const total = session.total;
    session.reporting = session.reporting.then(async () => {
      const token = await session.start;
      const data = listeningData();
      if (!token || !data) return;
      data.set("played_seconds", String(total));
      data.set("finish", String(finish));
      const response = await fetch("/web/music/listening/" + encodeURIComponent(token), {
        method: "POST", body: data, credentials: "same-origin", headers: { Accept: "application/json" },
      });
      if (!response.ok || response.redirected) return;
      const result = await response.json();
      session.counted = result.counted;
      if (result.counted && session === listening && historyFeedback) historyFeedback.textContent = historyFeedback.dataset.recorded;
    }).catch(() => {});
  }
  musicAudio.addEventListener("play", startListening);
  musicAudio.addEventListener("seeking", () => {
    if (!listening) return;
    listening.previous = musicAudio.currentTime;
    listening.wall = performance.now();
  });
  musicAudio.addEventListener("seeked", () => {
    if (!listening) return;
    listening.previous = musicAudio.currentTime;
    listening.wall = performance.now();
  });
  musicAudio.addEventListener("timeupdate", () => {
    if (!listening || musicAudio.paused) return;
    advanceListen();
    if (Date.now() - listening.lastReport >= 5000) reportListen(listening);
  });
  musicAudio.addEventListener("pause", () => {
    advanceListen();
    reportListen(listening, !current || listening?.entry !== current);
  });
  musicAudio.addEventListener("ended", () => {
    advanceListen();
    reportListen(listening, true);
  });
  window.addEventListener("pagehide", () => {
    if (!listening || !navigator.sendBeacon) return;
    advanceListen();
    const session = listening;
    session.start.then(token => {
      const data = listeningData();
      if (!token || !data) return;
      data.set("played_seconds", String(session.total)); data.set("finish", "true");
      navigator.sendBeacon("/web/music/listening/" + encodeURIComponent(token), data);
    });
  });
  const volumeKey = "plamenu-music-volume:" + device;
  const muteKey = "plamenu-music-muted:" + device;
  try {
    const saved = localStorage.getItem(volumeKey);
    if (saved !== null && Number.isFinite(Number(saved))) musicAudio.volume = Math.min(1, Math.max(0, Number(saved)));
    musicAudio.muted = localStorage.getItem(muteKey) === "true";
  } catch { /* Storage and software volume are optional on some devices. */ }
  musicAudio.addEventListener("volumechange", () => {
    try { localStorage.setItem(volumeKey, String(musicAudio.volume)); localStorage.setItem(muteKey, String(musicAudio.muted)); } catch {}
  });
  if ("mediaSession" in navigator) {
    const handlers = {
      play: () => musicAudio.play().catch(reportError),
      pause: () => musicAudio.pause(),
      nexttrack: () => { const data = formData("next"); if (data) submit(data, "next"); },
      previoustrack: () => { const data = formData("previous"); if (data) submit(data, "previous"); },
      seekto: detail => { if (Number.isFinite(detail.seekTime)) musicAudio.currentTime = detail.seekTime; },
    };
    for (const [action, handler] of Object.entries(handlers)) {
      try { navigator.mediaSession.setActionHandler(action, handler); } catch {}
    }
    musicAudio.addEventListener("play", () => { navigator.mediaSession.playbackState = "playing"; });
    musicAudio.addEventListener("pause", () => { navigator.mediaSession.playbackState = "paused"; });
  }
  const restoredPosition = Number(musicAudio.dataset.position);
  musicAudio.addEventListener("loadedmetadata", () => {
    if (restoredPosition > 0) musicAudio.currentTime = restoredPosition;
  }, { once: true });
  musicAudio.addEventListener("timeupdate", () => {
    if (!musicAudio.paused && Date.now() - lastSaved > 5000) savePosition();
  });
  musicAudio.addEventListener("pause", () => { if (!refreshing) savePosition(); });
  musicAudio.addEventListener("ended", () => {
    const data = formData("next");
    if (!data) return;
    data.set("ended", "true");
    submit(data, "next");
  });
  // An idle player also notices source revocation and edits. Reading a queue
  // has no listening-history side effects and cannot start idle playback.
  setInterval(() => {
    if (saving || refreshing) return;
    refreshing = true;
    pending = pending.catch(() => {}).then(() => refresh()).catch(() => {
      musicAudio.pause(); reportError();
    }).finally(() => { refreshing = false; });
  }, 15000);
  window.addEventListener("message", event => {
    if (event.origin !== location.origin || event.data?.type !== "plamenu-music-source") return;
    let source;
    try { source = new URL(event.data.url); } catch { return; }
    if (source.origin !== location.origin || source.pathname !== "/music/player") return;
    source.searchParams.set("device", device);
    pending = pending.catch(() => {}).then(() => refresh(undefined, source)).catch(reportError);
  });
  musicAudio.addEventListener('plamenu:music-command', event => {
    if (!['next','previous'].includes(event.detail)) return;
    const data = formData(event.detail);
    if (data) submit(data, event.detail);
  });
  musicAudio.addEventListener('plamenu:music-refresh', () => {
    pending = pending.catch(() => {}).then(() => refresh('select')).catch(reportError);
  });
  bindQueueForms();
}

}

bindMusicPlayer();

// Episode-page playback uses the same elapsed-time protocol as the persistent
// player. Loading metadata and requesting audio bytes never starts a session.
function bindPodcastEpisodes(root) {
root.querySelectorAll("audio[data-podcast-episode]:not([data-listening-bound])").forEach(audio => {
  audio.dataset.listeningBound = "true";
  let session = null;
  const fields = () => new URLSearchParams({ csrf: audio.dataset.csrf });
  function counted() {
    const state = document.querySelector(".podcast-played-state");
    if (state) state.textContent = state.dataset.played;
    const form = document.querySelector("form.podcast-played-toggle");
    if (form) {
      form.elements.namedItem("enabled").value = "false";
      const button = form.querySelector("button");
      if (button) button.textContent = button.dataset.played;
    }
  }
  function begin() {
    if (session && !session.finished) {
      session.previous = audio.currentTime; session.wall = performance.now();
      return;
    }
    const data = fields();
    data.set("request_key", crypto.randomUUID());
    data.set("episode_id", audio.dataset.podcastEpisode);
    session = { total: 0, previous: audio.currentTime, wall: performance.now(),
      lastReport: Date.now(), counted: false, finished: false, pending: Promise.resolve() };
    session.start = fetch("/web/music/listening/start", {
      method: "POST", body: data, credentials: "same-origin", headers: { Accept: "application/json" },
    }).then(async response => {
      if (!response.ok || response.redirected) return null;
      const result = await response.json();
      return result.enabled ? result.token : null;
    }).catch(() => null);
  }
  function advance() {
    if (!session || session.finished || audio.paused) return;
    const now = performance.now(), delta = audio.currentTime - session.previous;
    if (!audio.seeking && delta > 0 && delta < 5) session.total += Math.min(delta, Math.max(0, (now - session.wall) / 1000));
    session.previous = audio.currentTime; session.wall = now;
  }
  function report(finish = false) {
    const current = session;
    if (!current || current.finished || (!finish && current.counted)) return;
    if (finish) current.finished = true;
    current.lastReport = Date.now();
    const total = current.total;
    current.pending = current.pending.then(async () => {
      const token = await current.start;
      if (!token) return;
      const data = fields(); data.set("played_seconds", String(total)); data.set("finish", String(finish));
      const response = await fetch("/web/music/listening/" + encodeURIComponent(token), {
        method: "POST", body: data, credentials: "same-origin", headers: { Accept: "application/json" },
      });
      if (!response.ok || response.redirected) return;
      const result = await response.json();
      if (result.counted) { current.counted = true; counted(); }
    }).catch(() => {});
  }
  audio.addEventListener("play", begin);
  audio.addEventListener("timeupdate", () => { advance(); if (session && Date.now() - session.lastReport >= 5000) report(); });
  audio.addEventListener("seeking", () => { if (session) { session.previous = audio.currentTime; session.wall = performance.now(); } });
  audio.addEventListener("pause", () => report());
  audio.addEventListener("ended", () => report(true));
  window.addEventListener("pagehide", () => {
    if (!session || session.finished) return;
    const current = session; current.finished = true;
    current.start.then(token => {
      if (!token) return;
      const data = fields(); data.set("played_seconds", String(current.total)); data.set("finish", "true");
      navigator.sendBeacon("/web/music/listening/" + encodeURIComponent(token), data);
    });
  });
  // Native controls can start playback before this deferred script arrives.
  // Count only the actual playback observed from initialization onward.
  if (!audio.paused && !audio.ended) begin();
});
}
bindPodcastEpisodes(document);

})();

// Refresh a running private download; native refresh links remain usable.
{
  let panel = document.querySelector(".music-download-detail");
  const controller = new AbortController();
  let timer;
  let submitting = false;
  const pending = () => panel && ["queued", "running"].includes(panel.dataset.state);
  const schedule = () => { if (pending() && !controller.signal.aborted) timer = setTimeout(refreshDownload, 3000); };
  async function refreshDownload() {
    if (!pending() || controller.signal.aborted) return;
    if (document.hidden || submitting || panel.contains(document.activeElement) && document.activeElement.closest("form")) { schedule(); return; }
    try {
      const response = await fetch(location.href, {credentials: "same-origin", cache: "no-store", signal: controller.signal});
      if (!response.ok || response.redirected) return;
      const next = new DOMParser().parseFromString(await response.text(), "text/html").querySelector(".music-download-detail");
      if (!next) return;
      panel.replaceWith(next);
      panel = next;
      schedule();
    } catch { /* The native refresh link handles lost connections. */ }
  }
  if (panel) {
    document.addEventListener("submit", event => { if (panel.contains(event.target)) submitting = true; });
    window.addEventListener("pagehide", () => { clearTimeout(timer); controller.abort(); }, {once: true});
    schedule();
  }
}

// Discovery impressions reflect actual visibility, not delivered HTML.
(() => {
  const history = document.querySelector('[data-discovery-seen-csrf]');
  if (!history || !('IntersectionObserver' in window)) return;
  const selector = '[data-discovery-card],[data-discovery-post],[data-discovery-video]';
  const recorded = new Set(), pending = new Set(), timers = new Map();
  let flushTimer;
  async function flush() {
    const ids = [...pending].slice(0, 50); if (!ids.length) return;
    ids.forEach(id => pending.delete(id));
    try {
      const response = await fetch('/web/discover/seen', {method:'POST', credentials:'same-origin', keepalive:true,
        body:new URLSearchParams({csrf:history.dataset.discoverySeenCsrf, ids:ids.join(',')})});
      if (!response.ok) throw new Error('Impression not saved');
    } catch { ids.forEach(id => recorded.delete(id)); }
  }
  const observer = new IntersectionObserver(changes => {
    for (const change of changes) {
      const card = change.target;
      clearTimeout(timers.get(card)); timers.delete(card);
      if (!change.isIntersecting || change.intersectionRatio < 0.5 || document.hidden) continue;
      timers.set(card, setTimeout(() => {
        if (document.hidden || !card.isConnected) return;
        const id = card.dataset.discoveryCard || card.dataset.discoveryPost || card.dataset.discoveryVideo;
        if (recorded.has(id)) return;
        recorded.add(id); pending.add(id); observer.unobserve(card);
        clearTimeout(flushTimer); flushTimer=setTimeout(flush, 1200);
      }, 1000));
    }
  }, {threshold:[0,0.5]});
  function observe(root) {root.querySelectorAll(selector).forEach(card => observer.observe(card));}
  observe(document);
  const results=document.querySelector('[data-discovery-results]');
  if (results) new MutationObserver(() => observe(results)).observe(results,{childList:true});
  document.addEventListener('visibilitychange', () => {
    timers.forEach(clearTimeout); timers.clear();
    if (document.hidden) flush(); else {observer.disconnect(); observe(document);}
  });
  window.addEventListener('pagehide',flush);
})();
