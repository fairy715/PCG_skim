<script lang="ts">
  // Hover button beside a sender's address: one click files them in the
  // address book (fork addition). Shaped after CopyAffordance, which it sits
  // next to — same size, same reveal, same brief checkmark.
  import { api } from "../lib/api";
  import { t } from "../lib/i18n/index.svelte";

  interface Props {
    name: string | null;
    email: string;
  }

  let { name, email }: Props = $props();

  let justAdded = $state(false);
  let timer: number | undefined;

  async function onClick(e: MouseEvent) {
    // Don't let the click fall through to a parent that collapses the message.
    e.stopPropagation();
    e.preventDefault();
    try {
      await api.saveContact({ id: 0, name: name ?? "", email, company: null, phone: null, note: null });
    } catch {
      return; // not an address worth keeping; nothing to confirm
    }
    justAdded = true;
    if (timer !== undefined) clearTimeout(timer);
    timer = window.setTimeout(() => {
      justAdded = false;
      timer = undefined;
    }, 1200);
  }
</script>

<button
  type="button"
  class="copy-btn"
  class:copied={justAdded}
  aria-label={justAdded ? t("contacts.added") : t("contacts.add_sender")}
  title={justAdded ? t("contacts.added") : t("contacts.add_sender")}
  onclick={onClick}
>
  {#if justAdded}
    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M3.5 8.5l3 3 6-6" />
    </svg>
  {:else}
    <!-- Head and shoulders with a plus: add this person. -->
    <svg width="13" height="13" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <circle cx="6.5" cy="5" r="2.6" />
      <path d="M1.8 13.5c.5-2.6 2.4-4 4.7-4 1.2 0 2.3.4 3.1 1.1" />
      <path d="M12.3 9.2v4.6M10 11.5h4.6" />
    </svg>
  {/if}
</button>

<style>
  .copy-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 20px;
    height: 20px;
    padding: 0;
    margin-left: 2px;
    border: none;
    background: transparent;
    color: var(--text-faint);
    border-radius: var(--radius-s);
    cursor: pointer;
    opacity: 0;
    transition:
      opacity 0.08s,
      color 0.08s,
      background 0.08s;
    vertical-align: middle;
    flex-shrink: 0;
  }
  :global(.copy-host:hover) > .copy-btn,
  :global(.copy-host) > .copy-btn:focus-visible,
  .copy-btn.copied {
    opacity: 1;
  }
  .copy-btn:hover {
    color: var(--text-dim);
    background: var(--hover);
  }
  .copy-btn.copied {
    color: var(--text);
  }

  @media (prefers-reduced-motion: reduce) {
    .copy-btn {
      transition: none;
    }
  }
</style>
