<script lang="ts">
  // Address book (fork addition). One panel, shaped like Settings: a list you
  // can search, a row that opens into its own editor, and nothing else on
  // screen until it is asked for.
  import { api, errorCode, errorMessage } from "../lib/api";
  import type { Contact, ContactGroup } from "../lib/api";
  import { backdropClose } from "../lib/backdrop";
  import { t } from "../lib/i18n/index.svelte";
  import { mail } from "../lib/stores/mail.svelte";

  let { onclose }: { onclose: () => void } = $props();

  type Tab = "people" | "groups";
  let tab = $state<Tab>("people");
  let contacts = $state<Contact[]>([]);
  let groups = $state<ContactGroup[]>([]);
  let query = $state("");
  let notice = $state("");
  let error = $state("");

  // The row being edited: an id, 0 for a new entry, null for none.
  let editing = $state<number | null>(null);
  let form = $state<Contact>(blank());
  let confirmingDelete = $state(false);

  let editingGroup = $state<number | null>(null);
  let groupName = $state("");
  let groupMembers = $state<number[]>([]);
  let memberQuery = $state("");

  function blank(): Contact {
    return { id: 0, name: "", email: "", company: null, phone: null, note: null };
  }

  async function load() {
    [contacts, groups] = await Promise.all([api.listContacts(), api.listContactGroups()]);
  }

  $effect(() => {
    void load().catch((e) => (error = errorMessage(e)));
  });

  function matches(c: Contact, q: string): boolean {
    const needle = q.trim().toLowerCase();
    if (!needle) return true;
    return [c.name, c.email, c.company ?? ""].some((v) => v.toLowerCase().includes(needle));
  }

  const shown = $derived(contacts.filter((c) => matches(c, query)));
  const shownGroups = $derived(
    groups.filter((g) => g.name.toLowerCase().includes(query.trim().toLowerCase())),
  );
  const byId = $derived(new Map(contacts.map((c) => [c.id, c])));

  function label(c: Contact): string {
    return c.name || c.email;
  }

  function initial(text: string): string {
    return (Array.from(text.trim())[0] ?? "?").toUpperCase();
  }

  function showError(e: unknown) {
    const code = errorCode(e);
    if (code === "contact_email") error = t("contacts.err_email");
    else if (code === "contact_exists") error = t("contacts.err_exists");
    else if (code === "group_name") error = t("contacts.err_group_name");
    else if (code === "group_exists") error = t("contacts.err_group_exists");
    else if (code === "contacts_import") error = t("contacts.err_import_empty");
    else error = errorMessage(e);
  }

  function reset() {
    error = "";
    notice = "";
    confirmingDelete = false;
  }

  function switchTab(next: Tab) {
    tab = next;
    editing = null;
    editingGroup = null;
    query = "";
    reset();
  }

  // ---- people --------------------------------------------------------------

  function open(c: Contact | null) {
    reset();
    if (c && editing === c.id) {
      editing = null;
      return;
    }
    form = c ? { ...c } : blank();
    editing = c ? c.id : 0;
  }

  async function save() {
    reset();
    try {
      await api.saveContact({
        ...form,
        company: form.company || null,
        phone: form.phone || null,
        note: form.note || null,
      });
      editing = null;
      await load();
    } catch (e) {
      showError(e);
    }
  }

  async function remove() {
    if (!confirmingDelete) {
      confirmingDelete = true;
      return;
    }
    await api.deleteContacts([form.id]);
    editing = null;
    reset();
    await load();
  }

  /** Start a message to these addresses in a compose window. */
  async function write(to: string) {
    const draft = await api.createDraft(await mail.composeAccountId());
    await api.updateDraft({ ...draft, to });
    await api.openComposeWindow(draft.id);
    onclose();
  }

  // ---- groups --------------------------------------------------------------

  function openGroup(g: ContactGroup | null) {
    reset();
    if (g && editingGroup === g.id) {
      editingGroup = null;
      return;
    }
    groupName = g?.name ?? "";
    groupMembers = g ? [...g.memberIds] : [];
    memberQuery = "";
    editingGroup = g ? g.id : 0;
  }

  const candidates = $derived(
    memberQuery.trim()
      ? contacts.filter((c) => !groupMembers.includes(c.id) && matches(c, memberQuery)).slice(0, 6)
      : [],
  );

  function addMember(c: Contact) {
    groupMembers = [...groupMembers, c.id];
    memberQuery = "";
  }

  async function saveGroup() {
    reset();
    try {
      await api.saveContactGroup(editingGroup ?? 0, groupName, groupMembers);
      editingGroup = null;
      await load();
    } catch (e) {
      showError(e);
    }
  }

  async function removeGroup() {
    if (!confirmingDelete) {
      confirmingDelete = true;
      return;
    }
    if (editingGroup) await api.deleteContactGroup(editingGroup);
    editingGroup = null;
    reset();
    await load();
  }

  function groupAddresses(ids: number[]): string {
    return ids
      .map((id) => byId.get(id)?.email)
      .filter((e): e is string => !!e)
      .join(", ");
  }

  // ---- import / export -----------------------------------------------------

  async function importFile() {
    reset();
    try {
      const report = await api.importContacts();
      if (!report) return;
      notice = t("contacts.imported", {
        added: report.added,
        updated: report.updated,
        skipped: report.skipped,
      });
      await load();
    } catch (e) {
      showError(e);
    }
  }

  async function exportFile() {
    reset();
    try {
      const path = await api.exportContacts();
      if (path) notice = t("contacts.exported", { path });
    } catch (e) {
      showError(e);
    }
  }

  /** Bring a just-opened editor fully into view: a row near the bottom of the
   *  list would otherwise open its form below the fold. */
  function reveal(node: HTMLElement) {
    node.scrollIntoView({ block: "nearest" });
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key !== "Escape") return;
    e.stopPropagation();
    if (editing !== null || editingGroup !== null) {
      editing = null;
      editingGroup = null;
      reset();
    } else {
      onclose();
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#snippet contactForm()}
  <form
    class="editor"
    use:reveal
    onsubmit={(e) => {
      e.preventDefault();
      void save();
    }}
  >
    <label class="field">
      <span class="microlabel">{t("contacts.name")}</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input bind:value={form.name} autofocus spellcheck="false" autocomplete="off" />
    </label>
    <label class="field">
      <span class="microlabel">{t("contacts.email")}</span>
      <input bind:value={form.email} type="email" required spellcheck="false" autocomplete="off" />
    </label>
    <div class="pair">
      <label class="field">
        <span class="microlabel">{t("contacts.company")}</span>
        <input bind:value={form.company} spellcheck="false" autocomplete="off" />
      </label>
      <label class="field">
        <span class="microlabel">{t("contacts.phone")}</span>
        <input bind:value={form.phone} spellcheck="false" autocomplete="off" />
      </label>
    </div>
    <label class="field">
      <span class="microlabel">{t("contacts.note")}</span>
      <textarea bind:value={form.note} rows="2"></textarea>
    </label>
    {#if error}<div class="warn">{error}</div>{/if}
    <div class="actions">
      <button type="submit" class="ghost solid">{t("contacts.save")}</button>
      {#if form.id}
        <button type="button" class="ghost" onclick={() => write(form.email)}>
          {t("contacts.write")}
        </button>
        <span class="spacer"></span>
        <button type="button" class={confirmingDelete ? "danger" : "ghost"} onclick={remove}>
          {confirmingDelete ? t("contacts.confirm_delete") : t("contacts.delete")}
        </button>
      {/if}
    </div>
  </form>
{/snippet}

{#snippet groupForm()}
  <form
    class="editor"
    use:reveal
    onsubmit={(e) => {
      e.preventDefault();
      void saveGroup();
    }}
  >
    <label class="field">
      <span class="microlabel">{t("contacts.group_name")}</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input bind:value={groupName} autofocus required spellcheck="false" autocomplete="off" />
    </label>
    <div class="field">
      <span class="microlabel">{t("contacts.members")}</span>
      {#if groupMembers.length > 0}
        <div class="chips">
          {#each groupMembers as id (id)}
            {@const member = byId.get(id)}
            {#if member}
              <span class="chip">
                {label(member)}
                <button
                  type="button"
                  aria-label={t("contacts.remove_member")}
                  onclick={() => (groupMembers = groupMembers.filter((m) => m !== id))}
                >×</button>
              </span>
            {/if}
          {/each}
        </div>
      {/if}
      <input
        bind:value={memberQuery}
        placeholder={t("contacts.add_member")}
        spellcheck="false"
        autocomplete="off"
        onkeydown={(e) => {
          if (e.key === "Enter" && candidates.length > 0) {
            e.preventDefault();
            addMember(candidates[0]);
          }
        }}
      />
      {#each candidates as c (c.id)}
        <button type="button" class="candidate" onclick={() => addMember(c)}>
          <span class="strong">{label(c)}</span>
          {#if c.name}<span class="dim">{c.email}</span>{/if}
        </button>
      {/each}
    </div>
    {#if error}<div class="warn">{error}</div>{/if}
    <div class="actions">
      <button type="submit" class="ghost solid">{t("contacts.save")}</button>
      {#if editingGroup && groupMembers.length > 0}
        <button type="button" class="ghost" onclick={() => write(groupAddresses(groupMembers))}>
          {t("contacts.write")}
        </button>
      {/if}
      {#if editingGroup}
        <span class="spacer"></span>
        <button type="button" class={confirmingDelete ? "danger" : "ghost"} onclick={removeGroup}>
          {confirmingDelete ? t("contacts.confirm_delete") : t("contacts.delete")}
        </button>
      {/if}
    </div>
  </form>
{/snippet}

<div class="overlay" use:backdropClose={onclose}>
  <div class="panel">
    <header>
      <h2>{t("contacts.title")}</h2>
      <button class="close" onclick={onclose} aria-label={t("settings.close")}>
        <svg width="11" height="11" viewBox="0 0 10 10"><path d="M0 0L10 10M10 0L0 10" stroke="currentColor" stroke-width="1.2" /></svg>
      </button>
    </header>

    <div class="bar">
      <div class="tabs">
        <button class="tab" class:active={tab === "people"} onclick={() => switchTab("people")}>
          {t("contacts.people")}
          <span class="count">{contacts.length}</span>
        </button>
        <button class="tab" class:active={tab === "groups"} onclick={() => switchTab("groups")}>
          {t("contacts.groups")}
          <span class="count">{groups.length}</span>
        </button>
      </div>
      <input class="search" bind:value={query} placeholder={t("contacts.search")} spellcheck="false" />
    </div>

    <div class="body">
      {#if tab === "people"}
        {#if editing === 0}
          {@render contactForm()}
        {:else}
          <button class="ghost add" onclick={() => open(null)}>+ {t("contacts.new")}</button>
        {/if}

        {#each shown as c (c.id)}
          <button class="row" class:open={editing === c.id} onclick={() => open(c)}>
            <span class="avatar">{initial(label(c))}</span>
            <span class="grow">
              <span class="strong">{label(c)}</span>
              <span class="dim">{c.name ? c.email : ""}{c.company ? `${c.name ? " · " : ""}${c.company}` : ""}</span>
            </span>
          </button>
          {#if editing === c.id}
            {@render contactForm()}
          {/if}
        {:else}
          {#if editing !== 0}
            <p class="empty">{query ? t("contacts.none_found") : t("contacts.empty")}</p>
          {/if}
        {/each}
      {:else}
        {#if editingGroup === 0}
          {@render groupForm()}
        {:else}
          <button class="ghost add" onclick={() => openGroup(null)}>+ {t("contacts.new_group")}</button>
        {/if}

        {#each shownGroups as g (g.id)}
          <button class="row" class:open={editingGroup === g.id} onclick={() => openGroup(g)}>
            <span class="avatar square">{initial(g.name)}</span>
            <span class="grow">
              <span class="strong">{g.name}</span>
              <span class="dim">{t("contacts.member_count", { n: g.memberIds.length })}</span>
            </span>
          </button>
          {#if editingGroup === g.id}
            {@render groupForm()}
          {/if}
        {:else}
          {#if editingGroup !== 0}
            <p class="empty">{query ? t("contacts.none_found") : t("contacts.empty_groups")}</p>
          {/if}
        {/each}
      {/if}
    </div>

    <footer>
      <button class="ghost" onclick={importFile}>{t("contacts.import")}</button>
      <button class="ghost" onclick={exportFile} disabled={contacts.length === 0}>
        {t("contacts.export")}
      </button>
      {#if notice}
        <span class="dim note">{notice}</span>
      {:else if error && editing === null && editingGroup === null}
        <span class="warn note">{error}</span>
      {/if}
    </footer>
  </div>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.35);
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding-top: 9vh;
    z-index: 100;
  }
  .panel {
    width: 560px;
    max-width: calc(100vw - 48px);
    max-height: 78vh;
    background: var(--surface-raised);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-l);
    box-shadow: var(--shadow-pop);
    display: flex;
    flex-direction: column;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 16px 20px 12px;
  }
  h2 {
    font-size: 17px;
    font-weight: 800;
    letter-spacing: -0.02em;
  }
  .close {
    width: 28px;
    height: 28px;
    display: grid;
    place-items: center;
    border-radius: var(--radius-s);
    color: var(--text-dim);
  }
  .close:hover {
    background: var(--hover);
    color: var(--text);
  }

  .bar {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 0 20px 12px;
  }
  .tabs {
    display: flex;
    gap: 4px;
  }
  .tab {
    padding: 5px 10px;
    border-radius: var(--radius-s);
    font-size: 12.5px;
    color: var(--text-dim);
  }
  .tab:hover {
    background: var(--hover);
  }
  .tab.active {
    background: var(--hover);
    color: var(--text);
  }
  .count {
    margin-left: 4px;
    font-family: var(--font-mono);
    font-size: 10.5px;
    color: var(--text-faint);
  }
  .search {
    flex: 1;
    min-width: 0;
    padding: 6px 10px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-s);
    font-size: 13px;
    user-select: text;
  }
  .search:focus {
    border-color: var(--text-faint);
  }

  .body {
    overflow-y: auto;
    padding: 0 20px 12px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-height: 120px;
  }
  .add {
    align-self: flex-start;
    margin-bottom: 8px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 7px 8px;
    margin: 0 -8px;
    border-radius: var(--radius-s);
    text-align: left;
  }
  .row:hover,
  .row.open {
    background: var(--hover);
  }
  .grow {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
  }
  .strong {
    font-weight: 600;
    font-size: 13.5px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .dim {
    color: var(--text-faint);
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .avatar {
    width: 30px;
    height: 30px;
    border-radius: 50%;
    background: var(--selected);
    display: grid;
    place-items: center;
    font-weight: 700;
    font-size: 12.5px;
    flex-shrink: 0;
  }
  .avatar.square {
    border-radius: var(--radius-s);
  }
  .empty {
    padding: 20px 0;
    color: var(--text-faint);
    font-size: 12.5px;
    line-height: 1.5;
  }

  .editor {
    display: flex;
    flex-direction: column;
    gap: 12px;
    margin: 6px 0 12px 42px;
    scroll-margin: 12px;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    flex: 1;
    min-width: 0;
  }
  .field input,
  .field textarea {
    padding: 8px 10px;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-s);
    font-size: 13px;
    user-select: text;
    resize: vertical;
    font-family: inherit;
  }
  .field input:focus,
  .field textarea:focus {
    border-color: var(--text-faint);
  }
  .pair {
    display: flex;
    gap: 12px;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .spacer {
    flex: 1;
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 3px 4px 3px 9px;
    border-radius: 999px;
    background: var(--selected);
    font-size: 12px;
  }
  .chip button {
    width: 16px;
    height: 16px;
    border-radius: 50%;
    color: var(--text-dim);
    line-height: 1;
  }
  .chip button:hover {
    background: var(--hover);
    color: var(--text);
  }
  .candidate {
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 5px 8px;
    border-radius: var(--radius-s);
    text-align: left;
  }
  .candidate:hover {
    background: var(--hover);
  }

  footer {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 20px 16px;
    border-top: 1px solid var(--hairline);
  }
  .note {
    margin-left: 4px;
    white-space: normal;
    line-height: 1.4;
  }

  .ghost {
    padding: 6px 12px;
    border-radius: var(--radius-s);
    border: 1px solid var(--hairline-strong);
    font-size: 12.5px;
    color: var(--text-dim);
    flex-shrink: 0;
  }
  .ghost:hover:not(:disabled) {
    background: var(--hover);
    color: var(--text);
  }
  .ghost:disabled {
    opacity: 0.5;
  }
  .ghost.solid {
    color: var(--text);
    font-weight: 600;
  }
  .danger {
    padding: 6px 12px;
    border-radius: var(--radius-s);
    background: var(--danger);
    color: #fff;
    font-size: 12.5px;
    font-weight: 600;
    flex-shrink: 0;
  }
  .warn {
    color: var(--danger);
    font-size: 12.5px;
    line-height: 1.45;
  }
</style>
