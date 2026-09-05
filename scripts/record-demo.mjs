// Synthetic demo only: never reads the user's Codex sessions.
// Reduced records follow the 0.152.1 shapes exercised by src/model.rs and
// src/rollout.rs tests; this is not a captured producer session or schema test.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, appendFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import { StringDecoder } from 'node:string_decoder';

const root = fileURLToPath(new URL('../', import.meta.url));
const output = resolve(root, 'docs/demo.cast');
const agg = process.env.AGG ?? 'agg';
function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with ${result.status}`);
}
run(agg, ['--version']);
run('cargo', ['build', '--locked']);
const temporary = mkdtempSync(join(tmpdir(), 'agentop-demo-'));
const sessions = join(temporary, 'sessions');
mkdirSync(sessions);
const ordinals = new Map();
const base = Date.now() - 15_000;
function record(agent, type, payload, offset) {
  const ordinal = ordinals.get(agent) ?? 0;
  ordinals.set(agent, ordinal + 1);
  appendFileSync(join(sessions, `rollout-${agent}.jsonl`), JSON.stringify({
    timestamp: new Date(base + offset * 1000).toISOString(), ordinal, type, payload,
  }) + '\n');
}
function event(agent, type, fields, offset) {
  record(agent, 'event_msg', { type, ...fields }, offset);
}
function message(agent, text, offset) {
  event(agent, 'agent_message', { message: text }, offset);
}
function call(agent, id, command, offset) {
  record(agent, 'response_item', {
    type: 'function_call', call_id: id, name: 'exec_command',
    arguments: JSON.stringify({ cmd: command }),
  }, offset);
}
function returned(agent, id, offset) {
  record(agent, 'response_item', {
    type: 'function_call_output', call_id: id, output: 'Synthetic demo: command succeeded.',
  }, offset);
}
function initialise(agent, model, effort, task, offset) {
  record(agent, 'session_meta', {
    id: agent, session_id: 'demo-session', cli_version: '0.152.1',
    cwd: '/demo/synthetic-session', agent_path: agent === 'root' ? '/root' : `/root/${agent}`,
    ...(agent === 'root' ? {} : { parent_thread_id: 'root' }),
    timestamp: new Date(base).toISOString(),
  }, offset);
  record(agent, 'turn_context', { model, effort }, offset);
  event(agent, 'task_started', { turn_id: `demo-${agent}` }, offset);
  message(agent, `Task: ${task}`, offset + 1);
}

let child;
try {
  initialise('root', 'gpt-6-astra', 'high', 'Prepare a small Rust release.', 0);
  initialise('docs', 'gpt-5.6-luna', 'medium', 'Check the installation instructions.', 1);
  message('docs', 'Installation instructions checked.', 3);
  event('docs', 'task_complete', {}, 3);
  initialise('review', 'gpt-5.6-sol', 'high', 'Review the parser changes.', 4);
  call('review', 'demo-review', 'git diff -- src/parser.rs', 6);
  initialise('tests', 'gpt-5.6-luna', 'medium', 'Run the Rust tests and report the result.', 7);
  call('tests', 'demo-tests', 'cargo test --locked', 9);
  message('root', 'Tests and review are running; documentation is ready.', 10);

  const rows = 26;
  const cols = 116;
  const chunks = [JSON.stringify({
    version: 2, width: cols, height: rows,
    title: 'Agentop — synthetic demo', env: { TERM: 'xterm-256color' },
  }) + '\n'];
  const started = performance.now();
  const decoder = new StringDecoder('utf8');
  let capturing = true;
  const environment = { ...process.env };
  delete environment.NO_COLOR;
  child = spawn('script', ['-qefc',
    `stty cols ${cols} rows ${rows} -echo; exec ./target/debug/agentop --sessions-dir "$AGENTOP_DEMO_SESSIONS" --session demo-session`,
    '/dev/null'], {
    cwd: root, env: { ...environment, TERM: 'xterm-256color', AGENTOP_DEMO_SESSIONS: sessions,
      AGENTOP_SCHEMA_DIR: join(temporary, 'catalogue') },
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  let queryTail = '';
  child.stdout.on('data', data => {
    const text = decoder.write(data);
    // Answer crossterm's startup cursor query as a terminal emulator would.
    const queries = queryTail + text;
    if (queries.includes('\x1b[6n')) child.stdin.write('\x1b[1;1R');
    queryTail = queries.slice(-3);
    if (capturing && text) chunks.push(JSON.stringify([
      (performance.now() - started) / 1000, 'o', text,
    ]) + '\n');
  });
  const exited = new Promise((accept, reject) => {
    child.once('error', reject);
    child.once('exit', code => code === 0 ? accept() : reject(new Error(`TUI exited with ${code}`)));
  });
  // Attach immediately so an early process failure is never an unhandled rejection.
  exited.catch(() => {});
  const pause = ms => new Promise(accept => setTimeout(accept, ms));
  const key = text => {
    if (child.exitCode !== null) throw new Error(`TUI exited with ${child.exitCode}: ${chunks.slice(1).map(line => JSON.parse(line)[2]).join('')}`);
    child.stdin.write(text);
  };
  await pause(3000);
  key('j');                         // Most recently active child: tests.
  await pause(2000);
  key('\r');                        // Open interactions.
  await pause(2000);
  key('k');                         // Highlight the readable task announcement.
  await pause(4000);
  returned('tests', 'demo-tests', (Date.now() - base) / 1000);
  message('tests', 'All Rust tests passed. No failures.', (Date.now() - base) / 1000);
  event('tests', 'task_complete', {}, (Date.now() - base) / 1000);
  await pause(4000);
  key('jj');                        // Read the result.
  await pause(3000);
  key('\x1b');                      // Return to the live tree.
  await pause(3000);
  returned('review', 'demo-review', (Date.now() - base) / 1000);
  message('review', 'Review complete. No blocking findings.', (Date.now() - base) / 1000);
  event('review', 'task_complete', {}, (Date.now() - base) / 1000);
  message('root', 'Tests, review and documentation are complete.', (Date.now() - base) / 1000);
  event('root', 'task_complete', {}, (Date.now() - base) / 1000);
  await pause(3000);
  capturing = false;                // Omit terminal teardown from the loop.
  key('q');
  child.stdin.end();
  await exited;
  writeFileSync(output, chunks.join(''));
  run(agg, ['--theme', 'github-dark', '--font-family', 'DejaVu Sans Mono',
    '--font-size', '16', '--line-height', '1.3', '--fps-cap', '10',
    '--last-frame-duration', '2', output, resolve(root, 'docs/demo.gif')]);
  console.log('Recorded docs/demo.cast and docs/demo.gif using synthetic data only.');
} finally {
  if (child && child.exitCode === null) child.kill('SIGTERM');
  // Only the private directory created by mkdtemp above is removed.
  rmSync(temporary, { recursive: true });
}
