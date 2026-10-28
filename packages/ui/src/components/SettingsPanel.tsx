import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useRef, useState } from "react";
import {
	formatShortcut,
	isMac,
	loadShortcut,
	recordKey,
	type ShortcutInfo,
	saveShortcut,
} from "../shortcut.ts";
import { deviceLabel, deviceValue, missingChoice } from "../voice-device.ts";
import { setVoiceLanguage, VOICE_LANGUAGES, voiceLanguage } from "../voice-language.ts";
import { loadVoiceInputOptions, onVoiceDevices, saveVoiceInput, type VoiceInputOptions } from "../voice-native.ts";
import "../settings.css";

type Status =
	| { kind: "idle" }
	| { kind: "recording" }
	| { kind: "applying"; shortcut: string }
	| { kind: "applied" }
	| { kind: "failed"; reason: string };

type SaveStatus =
	| { kind: "idle" }
	| { kind: "saving" }
	| { kind: "saved" }
	| { kind: "failed"; reason: string };

function reasonOf(error: unknown, fallback = "Gentle Dot could not change the shortcut."): string {
	if (typeof error === "string" && error) return error;
	if (error instanceof Error && error.message) return error.message;
	return fallback;
}

/** The wake word's state (S30.7): whether it is on, and if not, why it cannot be. */
type WakeWord = { enabled: boolean; available: boolean; reason?: string; phrase?: string };

const NO_WAKE_WORD = "The wake word is not available on this computer.";

async function wakeWordStatus(): Promise<WakeWord> {
	try {
		const status = await invoke<Partial<WakeWord> | undefined>("wake_word_status");
		return {
			enabled: status?.enabled === true,
			available: status?.available === true,
			reason: typeof status?.reason === "string" ? status.reason : undefined,
			phrase: typeof status?.phrase === "string" ? status.phrase : undefined,
		};
	} catch {
		return { enabled: false, available: false, reason: NO_WAKE_WORD };
	}
}

function wakeWordReason(error: unknown): string {
	if (typeof error === "string" && error) return error;
	if (error instanceof Error && error.message) return error.message;
	return "Gentle Dot could not change the wake word.";
}

/** The desktop panel's settings (S33.1); for now, the shortcut that opens the panel. */
export function SettingsPanel({ close }: { close: () => void }) {
	const [language, setLanguage] = useState(() => voiceLanguage() ?? "");
	const [wake, setWake] = useState<WakeWord | undefined>(undefined);
	const [wakeError, setWakeError] = useState<string | undefined>(undefined);
	const [phrase, setPhrase] = useState("");
	const mac = isMac();
	const show = (accelerator: string) => formatShortcut(accelerator, mac);

	/** Saves the words the detector listens for, and shows the app's reason when it refuses. */
	const savePhrase = useCallback(async () => {
		setWakeError(undefined);
		try {
			const status = await invoke<Partial<WakeWord>>("wake_phrase_set", { phrase });
			if (live.current) setWake({ ...(status as WakeWord) });
		} catch (error) {
			if (live.current) setWakeError(wakeWordReason(error));
		}
	}, [phrase]);
	const [info, setInfo] = useState<ShortcutInfo | undefined>(undefined);
	const [loadFailed, setLoadFailed] = useState(false);
	const [status, setStatus] = useState<Status>({ kind: "idle" });
	const [input, setInput] = useState<VoiceInputOptions | undefined>(undefined);
	const [inputLoadFailed, setInputLoadFailed] = useState(false);
	const [inputStatus, setInputStatus] = useState<SaveStatus>({ kind: "idle" });
	const live = useRef(true);
	const prompt = useRef<HTMLSpanElement>(null);

	useEffect(() => {
		live.current = true;
		loadShortcut().then(
			(found) => {
				if (live.current) setInfo(found);
			},
			() => {
				if (live.current) setLoadFailed(true);
			},
		);
		loadVoiceInputOptions().then(
			(found) => {
				if (live.current) setInput(found);
			},
			() => {
				if (live.current) setInputLoadFailed(true);
			},
		);
		// The app pushes the whole list whenever the Mac's devices change (S1): the picker follows
		// the system from then on, instead of the snapshot it read when it opened.
		let stopDevices: (() => void) | undefined;
		void onVoiceDevices((pushed) => {
			if (!live.current) return;
			setInput(pushed);
			setInputLoadFailed(false);
		}).then((stop) => {
			stopDevices = stop;
		});
		return () => {
			live.current = false;
			stopDevices?.();
		};
	}, []);

	const apply = useCallback((accelerator: string) => {
		setStatus({ kind: "applying", shortcut: accelerator });
		saveShortcut(accelerator).then(
			(saved) => {
				if (!live.current) return;
				setInfo((current) => current && { ...current, shortcut: saved });
				setStatus({ kind: "applied" });
			},
			(error: unknown) => {
				if (live.current) setStatus({ kind: "failed", reason: reasonOf(error) });
			},
		);
	}, []);

	useEffect(() => {
		if (!isTauri()) return;
		let live = true;
		wakeWordStatus().then((found) => {
			if (!live) return;
			setWake(found);
			setPhrase(found.phrase ?? "");
		});
		// The backend's own state, pushed on every change: the switch shows what is true, not what
		// it last asked for.
		let stopListening: (() => void) | undefined;
		void listen<{ wake?: Partial<WakeWord> }>("voice://state", (event) => {
			const pushed = event.payload?.wake;
			if (!live || !pushed) return;
			setWake((previous) => ({
				enabled: pushed.enabled === true,
				available: pushed.available !== false,
				reason: typeof pushed.reason === "string" ? pushed.reason : undefined,
				phrase: typeof pushed.phrase === "string" ? pushed.phrase : previous?.phrase,
			}));
		}).then((stop) => {
			stopListening = stop;
		});
		return () => {
			live = false;
			stopListening?.();
		};
	}, []);

	const setWakeWord = useCallback((enabled: boolean) => {
		invoke<WakeWord>("wake_word_set", { enabled }).then(
			(status) => {
				if (!live.current) return;
				setWake(status);
				setWakeError(undefined);
			},
			(error: unknown) => {
				if (live.current) setWakeError(wakeWordReason(error));
			},
		);
	}, []);

	const recording = status.kind === "recording";
	useEffect(() => {
		if (!recording) return;
		prompt.current?.focus();
		// Captured before the panel's own keys: Esc here cancels recording instead of hiding the
		// panel, and ⌘⇧F is recorded instead of filling the screen (they skip handled keys).
		const onKey = (event: KeyboardEvent) => {
			event.preventDefault();
			const recorded = recordKey(event);
			if (recorded.kind === "cancel") setStatus({ kind: "idle" });
			else if (recorded.kind === "shortcut") apply(recorded.accelerator);
		};
		window.addEventListener("keydown", onKey, true);
		return () => window.removeEventListener("keydown", onKey, true);
	}, [recording, apply]);

	const applying = status.kind === "applying";

	const applyInput = useCallback((uid: string | null) => {
		setInputStatus({ kind: "saving" });
		saveVoiceInput(uid).then(
			(chosen) => {
				if (!live.current) return;
				setInput((current) => current && { ...current, chosen });
				setInputStatus({ kind: "saved" });
			},
			(error: unknown) => {
				if (live.current)
					setInputStatus({
						kind: "failed",
						reason: reasonOf(error, "Gentle Dot could not change the microphone."),
					});
			},
		);
	}, []);

	const inputSaving = inputStatus.kind === "saving";
	return (
		<section className="accounts settings" aria-label="Settings">
			<header className="accounts-header">
				<h2>Settings</h2>
				<button type="button" className="icon" aria-label="Close settings" onClick={close}>
					×
				</button>
			</header>

			<section className="provider-section" aria-labelledby="settings-shortcut">
				<h3 id="settings-shortcut">Keyboard shortcut</h3>
				<p className="muted">Opens and hides the panel from any app.</p>
				{loadFailed ? (
					<p className="settings-error" role="alert">
						Gentle Dot could not read the current shortcut.
					</p>
				) : null}
				<div className="shortcut-row">
					{recording ? (
						<span ref={prompt} className="shortcut-recording" tabIndex={-1}>
							Press the new shortcut… Esc cancels.
						</span>
					) : (
						<kbd className="shortcut-keys">{info ? show(info.shortcut) : "…"}</kbd>
					)}
					{recording ? (
						<button type="button" onClick={() => setStatus({ kind: "idle" })}>
							Cancel
						</button>
					) : (
						<button
							type="button"
							className="primary"
							disabled={!info || applying}
							onClick={() => setStatus({ kind: "recording" })}
						>
							Change
						</button>
					)}
				</div>
				{status.kind === "applying" ? (
					<p className="muted" role="status">
						Applying {show(status.shortcut)}…
					</p>
				) : null}
				{status.kind === "applied" ? (
					<p className="settings-ok" role="status">
						Saved. It works right away.
					</p>
				) : null}
				{status.kind === "failed" ? (
					<p className="settings-error" role="alert">
						{status.reason}
					</p>
				) : null}
				<p className="settings-hint muted">
					Use at least one modifier key ({mac ? "⌘ ⌥ ⌃ ⇧" : "Super, Alt, Ctrl, Shift"}), or a function key.
				</p>
				<button
					type="button"
					className="link"
					title={info ? `Default: ${show(info.default)}` : undefined}
					disabled={!info || info.shortcut === info.default || applying || recording}
					onClick={() => info && apply(info.default)}
				>
					Reset to default
				</button>
			</section>

			<section className="provider-section" aria-labelledby="settings-microphone">
				<h3 id="settings-microphone">Microphone</h3>
				<p className="muted">
					The input the voice notes are recorded from. Changing it applies to the next recording.
				</p>
				{inputLoadFailed ? (
					<p className="settings-error" role="alert">
						Gentle Dot could not read the current microphone.
					</p>
				) : (
					<select
						aria-label="Microphone"
						value={input ? deviceValue(input) : ""}
						disabled={!input || inputSaving}
						onChange={(event) => applyInput(event.target.value || null)}
					>
						<option value="">System default</option>
						{input?.devices.map((device) => (
							<option key={device.uid} value={device.uid}>
								{deviceLabel(device)}
							</option>
						))}
					</select>
				)}
				{input && missingChoice(input) ? (
					<p className="settings-error" role="alert">
						The chosen microphone is not connected. The system default is used instead.
					</p>
				) : null}
				{inputStatus.kind === "saved" ? (
					<p className="settings-ok" role="status">
						Saved. It applies to the next recording.
					</p>
				) : null}
				{inputStatus.kind === "failed" ? (
					<p className="settings-error" role="alert">
						{inputStatus.reason}
					</p>
				) : null}
			</section>

			<section className="provider-section" aria-labelledby="settings-voice-language">
				<h3 id="settings-voice-language">Voice language</h3>
				<p className="muted">
					The language macOS speech listens in. The local voice model detects it by itself.
				</p>
				<select
					aria-label="Voice language"
					value={language}
					onChange={(event) => {
						setLanguage(event.target.value);
						setVoiceLanguage(event.target.value);
					}}
				>
					{VOICE_LANGUAGES.map(([locale, label]) => (
						<option key={locale || "auto"} value={locale}>
							{label}
						</option>
					))}
				</select>
			</section>

			<section className="provider-section" aria-labelledby="settings-wake-word">
				<h3 id="settings-wake-word">Wake word</h3>
				<p className="muted">
					Say “oye Doti” and the note starts on its own. It ends after a moment of silence.
				</p>
				<label>
					<input
						type="checkbox"
						aria-label="Listen for the wake word"
						checked={wake?.enabled === true}
						disabled={wake?.available !== true}
						onChange={(event) => setWakeWord(event.target.checked)}
					/>
					<span>Listen for “{wake?.phrase ?? "oye Doti"}”</span>
				</label>
				<label className="settings-phrase">
					<span>Words to listen for</span>
					<input
						type="text"
						aria-label="Words to listen for"
						value={phrase}
						maxLength={60}
						disabled={wake === undefined}
						onChange={(event) => setPhrase(event.target.value)}
						onKeyDown={(event) => {
							if (event.key === "Enter") void savePhrase();
						}}
					/>
				</label>
				<button type="button" disabled={wake === undefined || phrase === (wake?.phrase ?? "")} onClick={() => void savePhrase()}>
					Save the words
				</button>
				<p className="settings-hint muted">One to four words, two letters or more each. It takes effect on the spot.</p>
				{wake?.available === false && wake.reason ? (
					<p className="settings-hint muted">{wake.reason}</p>
				) : null}
				{wakeError ? (
					<p className="settings-error" role="alert">
						{wakeError}
					</p>
				) : null}
			</section>
		</section>
	);
}
