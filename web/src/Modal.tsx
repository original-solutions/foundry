import { Dialog } from "radix-ui";
import { isValidElement, type ReactNode } from "react";

/** The shared button's shape and size, without its colours: a button with a tone of its own adds a border, a hover and a text colour to it. */
export const buttonShape = "rounded border px-2 py-0.5 text-xs disabled:opacity-50";
export const button = `${buttonShape} border-neutral-600 hover:bg-neutral-800`;
/** Beside `button` on a toggle: its look while `aria-pressed` is true. */
export const pressed = "aria-pressed:border-sky-400/50 aria-pressed:bg-sky-400/10 aria-pressed:text-sky-200";
/** The shared button for the one action a form is for. */
export const primaryButton =
  "rounded border border-emerald-400 bg-emerald-400 px-2 py-0.5 text-xs font-semibold text-neutral-950 hover:border-emerald-300 hover:bg-emerald-300 disabled:opacity-50";
/** `primaryButton` for an action that destroys something: ending a terminal, archiving work that would be lost. */
export const dangerButton =
  "rounded border border-red-400 bg-red-400 px-2 py-0.5 text-xs font-semibold text-neutral-950 hover:border-red-300 hover:bg-red-300 disabled:opacity-50";
/**
 * The shared field look, without a shape or a size: a dark box whose border lightens on focus,
 * beside the browser's focus ring. A field shaped its own way (a reply beside a send button, a
 * pill) is built from it, so every field keeps one focus treatment.
 */
export const fieldLook = "border border-neutral-600 bg-neutral-950 text-neutral-100 placeholder:text-neutral-600 focus:border-neutral-400";
const box = `rounded ${fieldLook}`;
export const field = `mt-1 w-full ${box} px-2 py-1 text-sm`;
/** `field` inside a line of small text. */
export const inlineField = `${box} px-1.5 py-0.5 font-mono`;

/**
 * A dialog: the overlay, the centred panel, its title and description, then `children`. With a
 * `trigger`, the button that opens it too: text gets the shared button look, an element is used as is. `wide` gives a form with more to show more room.
 */
export function Modal({
  open,
  onOpenChange,
  trigger,
  title,
  description,
  wide,
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  trigger?: ReactNode;
  title: ReactNode;
  description: ReactNode;
  wide?: boolean;
  children: ReactNode;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      {isValidElement(trigger) ? (
        <Dialog.Trigger asChild>{trigger}</Dialog.Trigger>
      ) : (
        trigger !== undefined && <Dialog.Trigger className={button}>{trigger}</Dialog.Trigger>
      )}
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-50 bg-black/60" />
        <Dialog.Content className={`fixed left-1/2 top-1/2 z-50 max-h-[calc(100dvh-1.5rem)] overflow-y-auto ${wide ? "w-[min(32rem,calc(100vw-1.5rem))] rounded-xl border border-neutral-700/70 p-5" : "w-96 rounded p-4"} -translate-x-1/2 -translate-y-1/2 bg-neutral-900 text-sm text-neutral-100 shadow-xl`}>
          <Dialog.Title className="font-semibold">{title}</Dialog.Title>
          <Dialog.Description className="mt-1 text-neutral-400">{description}</Dialog.Description>
          {children}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
