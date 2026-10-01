import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as pause } from 'node:timers/promises';

// Headless Firefox needs an audio backend even when every sample goes to a
// MediaStreamDestination. This is test infrastructure, never Plamenu runtime.
export default async function setup() {
  if (process.env.A11Y_NULL_AUDIO !== '1') return;
  const directory = mkdtempSync(join(tmpdir(), 'plamenu-test-audio-'));
  const socket = join(directory, 'native');
  const child = spawn('pulseaudio', [
    '--daemonize=no', '--exit-idle-time=-1', '--use-pid-file=no', '-n',
    '--load=module-null-sink sink_name=plamenu_test',
    `--load=module-native-protocol-unix socket=${socket} auth-anonymous=1`,
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  let diagnostic = '';
  child.stderr.on('data', data => { diagnostic = (diagnostic + data).slice(-4096); });
  child.on('error', error => { diagnostic = String(error); });
  for (let attempt = 0; attempt < 100; attempt++) {
    if (existsSync(socket)) {
      process.env.PULSE_SERVER = `unix:${socket}`;
      return () => { child.kill('SIGTERM'); };
    }
    if (child.exitCode !== null || child.signalCode !== null) break;
    await pause(50);
  }
  child.kill('SIGTERM');
  throw new Error(`Headless test audio backend failed: ${diagnostic}`);
}
