# TODO — design review findings

設計レビュー（2026-06）で挙がった改善点。重要度順。`file:line` は指摘時点の目安。

## 🔴 Critical — 主張と実装の乖離 / 安全性

- [x] **C1. 「git = memory」が実装に存在しない** — README/manual が "git = the loop's
      memory" と謳うが `git init/add/commit` はコードに皆無。
      → *対応済み: 主張を「memory is the files in the data dir」に撤回（#feat/remove-looop-run）。*
      将来 git 履歴（各tick=1コミット、ロールバック、`looop diff`）を入れるなら別タスクで再検討。
- [x] **C3. single-writer 不変条件を `looop run` が破る** — pulse 稼働中でも拒否せず
      並走し journal/goals/PLAYBOOK をレース。
      → *対応済み: `looop run <goal>` を全廃（#feat/remove-looop-run）。*
- [x] **C2. RULE 1「1 tick = 1 move」の強制** — typed-action 化で解決。decider は
      `.decision.json` に1アクションを emit し、**looop が唯一の executor**。モデルが
      暴走しても tick あたり1 move にコードで限定される（#feat/typed-action-executor）。
      → *これで十分とし、完全サンドボックス化（下記）は **キャンセル**。*
      ~~残タスク（完全サンドボックス化）：~~
      - ~~`run_shell` をコードでゲート（deny-list：rm -rf / git push / gh pr merge / kubectl delete 等）~~
      - ~~decider のツール権限を落とす（stage B：調査は sensor、出力は .decision.json のみ）~~
      - ~~`--dangerously-skip-permissions` を外す（上記が済めば不要に）~~

## 🟠 High — コスト暴走 / 堅牢性

- [x] **H1. 失敗 tick にバックオフがない** — `.last-tick-hash` は成功時のみ書込のため、
      毎回失敗する tick が cadence ごとに無限リトライ＆無限課金。
      → *対応済み: `.tick-backoff` 状態ファイルに「同一 world hash での連続失敗数」を記録し、
      指数バックオフ（60s·2^(n-1)、上限1h）。beat の成功判定を「usable な decision を emit
      した時のみ hash commit」に厳密化（runner crash / 不正 decision / no-decision は全て
      失敗扱いで backoff、hash 未commit）。`src/tick.rs`*
- [x] **H2. 予算上限 / サーキットブレーカがない** — `looop cost` で可視化のみ。
      → *対応済み: config `max_daily_usd`（正の値で有効、既定 off）。tick は AI 呼び出し前に
      ledger の当日合計を集計し、上限到達で skip + `tick.budget` warn イベント。ローカル深夜で
      リセット。F3 と統合。`src/cost.rs`（spent_today/daily_budget）, `src/tick.rs`*
- [x] **H3. claude ランナーの tick コストが計上されない** — pi は `| looop _fmt` を通すが
      claude tick は通さないため常にゼロ行。runner 間で会計挙動が非対称。
      → *対応済み: `_fmt` を claude の stream-json `result.total_cost_usd` も計上できるよう拡張
      （pi=per-message 加算 / claude=累計値採用）。default claude tick を
      `--output-format stream-json --verbose | _fmt` に変更し両 runner で対称に計上。
      `src/cost.rs`, `src/config.rs`（doc に非対称の経緯も明記）*
- [x] **H4. `looop tick` が snapshots を共有してレースする** — `tick()`/`cmd_tick` は共有
      `snapshots/` を wipe→再生成。二重起動 / cron 重複で互いの snapshots を消し合う。ロック未取得。
      → *対応済み: (1) lock 取得を `run::acquire_lock`（atomic mkdir + PID liveness 再利用）に
      整理。(2) その後 `looop tick` サブコマンド自体を廃止（pulse は `tick()` をプロセス内で
      直接呼ぶので subcommand は不要）。beat runner は単一インスタンスの pulse のみになり、
      共有 snapshots のレースは原理的に消滅。`src/run.rs`, `src/main.rs`, `src/tick.rs`*

## 🟡 Medium — 整合性 / 設計

- [x] **M1. 環境変数の名前空間汚染** — `export_env` が裸の `CONFIG` / `CLAIMS_DIR` /
      `REPORTS_DIR` / `COST_LEDGER` を全子プロセスに export。特に `CONFIG` は衝突の温床。
      → *対応済み: `LOOOP_CONFIG` / `LOOOP_CLAIMS_DIR` / `LOOOP_REPORTS_DIR` /
      `LOOOP_COST_LEDGER` に統一。プロンプト/CONTRACT は相対パス＋`$LOOOP_BIN`参照で
      影響なし。`LOOOP_CONFIG` export は子の config をプロファイルに固定する副次効果も。`src/main.rs`*
- [x] **M2. `setup` ブートストラップが死んだ corpse で詰まる** — starter PLAYBOOK の
      "if a session setup already exists, do nothing" が Exited/Killed の corpse も
      「exists」と解釈し最大 retention(3d) 停止しうる。
      → *対応済み: seed PLAYBOOK を「RUNNING な setup のみ block、dead corpse は無視して
      新規開始（`looop ls` で state 確認）」に厳密化。`src/seed/PLAYBOOK.md`*
- [x] **M3. PLAYBOOK 編集は wake するが sensor 編集は wake しない（非対称）** —
      `world_hash` は sensors/*.sh をハッシュしない。
      → *対応済み: 意図的な挙動として `src/worldhash.rs` の doc コメントと `manual.txt`
      の ONE BEAT に明記（sensor script 編集は snapshot が変わるまで wake しない）。*
- [x] **M4. 判断は弱モデル・実行は強モデル** — tick=sonnet/low、worker=opus/medium。
      → *対応済み: pi の default tick を `claude-opus-4-8 --thinking low` に格上げ
      （最重要の一手を最強モデルが下す）。worker は opus/medium 据え置き。world-hash
      gate と1手のみの emit でコストは限定。config コメントに rationale 明記。`src/config.rs`*
- [x] **M5. config がプロファイル非スコープ** — `LOOOP_DATA_DIR` で分離してもconfigは共有。
      → *対応済み: config を `<data_dir>/looop.json` に移動。プロファイルを分ければ
      config も自動で分かれる。明示の `$LOOOP_CONFIG` は引き続き最優先(配線共有用)。`src/paths.rs`*

## 🟢 Feature / UX

- [x] **F1. `looop journal [--tail N]`** — 意思決定ログを一級コマンドで読む（今は cat か
      `log pulse`=イベント列のみ）。
      → *対応済み: `src/journal.rs`（`cmd_journal`、`--tail N`/`-n N`）。help/README/
      補完（zsh/bash）に追加。あわせて既に廃止した `tick` サブコマンドを補完からも除去。*
- [ ] **F2. `looop tick --dry-run`** — プロンプトを組み立てて表示するだけで LLM を呼ばない
      （課金なしでプロンプト調整）。
- [x] **F3. `max_daily_usd` サーキットブレーカ** — H2 と統合。→ *H2 で実装済み。*
- [ ] **F4. `looop doctor`** — runner が PATH にあるか / config が parse できるか /
      データディレクトリ書込可か を診断。
- [ ] **F5. オンボーディング — runner 自動検出** — default が `pi` 固定 + モデル名ハードコード。
      claude しか無いユーザは config 編集必須。初回に検出。
