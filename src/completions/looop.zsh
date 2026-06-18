#compdef looop

# Resolve looop's data dir the same way paths.rs does:
#   $LOOOP_DATA_DIR  or  ${XDG_STATE_HOME:-$HOME/.local/state}/looop
__looop_data_dir() {
    local default="${XDG_STATE_HOME:-$HOME/.local/state}/looop"
    print -r -- "${LOOOP_DATA_DIR:-$default}"
}

# Resolve the babysit fleet root the same way paths.rs does: a non-default
# profile (distinct LOOOP_DATA_DIR) gets its own <data>/babysit root; the
# default profile honors $BABYSIT_DIR, else ~/.babysit.
__looop_babysit_root() {
    local default="${XDG_STATE_HOME:-$HOME/.local/state}/looop"
    local data="${LOOOP_DATA_DIR:-$default}"
    if [[ "$data" != "$default" ]]; then
        print -r -- "$data/babysit"
    else
        print -r -- "${BABYSIT_DIR:-$HOME/.babysit}"
    fi
}

# Goal ids = goals/<id>.md basenames (also goals/archive/<id>.md).
__looop_goals() {
    local -a goals
    local data g name
    data=$(__looop_data_dir)
    for g in "$data"/goals/*.md(N) "$data"/goals/archive/*.md(N); do
        name=${g:t}
        name=${name%.md}
        [[ -n "$name" ]] && goals+=("$name")
    done
    (( ${#goals} )) && _describe 'goal' goals
}

# Worker session ids = looop-* under the babysit fleet root, minus the pulse.
__looop_workers() {
    local -a workers
    local root s name
    root=$(__looop_babysit_root)
    for s in "$root"/sessions/looop-*(N/); do
        name=${s:t}
        [[ "$name" == "looop-pulse" ]] && continue
        [[ -n "$name" ]] && workers+=("$name")
    done
    (( ${#workers} )) && _describe 'worker' workers
}

# Like __looop_workers but also includes the pulse (for read/observe verbs).
__looop_sessions() {
    local -a sessions
    local root s name
    root=$(__looop_babysit_root)
    for s in "$root"/sessions/looop-*(N/); do
        name=${s:t}
        [[ -n "$name" ]] && sessions+=("$name")
    done
    sessions+=('pulse')
    (( ${#sessions} )) && _describe 'session' sessions
}

_looop() {
    local curcontext="$curcontext" state line
    typeset -A opt_args

    _arguments -C \
        '1: :->cmd' \
        '*:: :->args'

    case $state in
        cmd)
            local -a cmds
            cmds=(
                'up:Run the pulse as a detached background service'
                'down:Stop the detached pulse service'
                'watch:Follow a session output read-only (tail -f)'
                'run:Force ONE goal now (manual override)'
                'tick:Run a single beat and exit (debug / cron)'
                'ls:List this profile worker sessions'
                'status:Structured snapshot of the loop state'
                'log:Show / tail / grep / follow a session output'
                'shot:Render a session current visible screen'
                'send:Type text into a session stdin'
                'key:Send named keys to a session'
                'expect:Block until a regex appears in output'
                'wait:Block until a session exits'
                'wait-idle:Block until output is quiet'
                'resize:Resize a session terminal'
                'restart:Restart the wrapped command'
                'start-session:Start a worker session'
                'attach:Attach to a waiting worker'
                'detach:Force-detach any other terminal'
                'kill:Terminate a worker session'
                'flag:Raise a worker attention flag'
                'unflag:Clear a worker attention flag'
                'prune:Clear finished worker corpses'
                'cost:Report LLM spend from the cost ledger'
                'config:Output shell integration (eval "$(looop config zsh)")'
                'version:Print the looop version'
                'help:Show the full design manual'
            )
            _describe 'command' cmds
            ;;
        args)
            case $words[1] in
                run)
                    (( CURRENT == 2 )) && __looop_goals
                    ;;
                up)
                    _arguments \
                        '(-w --watch)'{-w,--watch}'[Follow the pulse output after starting]' \
                        '--json[Pulse emits NDJSON to its log]'
                    ;;
                attach|kill|flag|unflag|restart)
                    (( CURRENT == 2 )) && __looop_workers
                    ;;
                watch|detach)
                    (( CURRENT == 2 )) && __looop_sessions
                    ;;
                log)
                    if (( CURRENT == 2 )); then
                        __looop_sessions
                    else
                        _arguments \
                            '--tail[Last N lines]:n:' \
                            '--grep[Only lines matching regex]:regex:' \
                            '--since[Only bytes after this offset]:bytes:' \
                            '(-f --follow)'{-f,--follow}'[Stream new output live]' \
                            '--raw[Include raw ANSI escapes]' \
                            '--json[Emit JSON]'
                    fi
                    ;;
                shot)
                    if (( CURRENT == 2 )); then
                        __looop_sessions
                    else
                        _arguments \
                            '--ansi[Keep ANSI color escapes]' \
                            '--json[Structured JSON output]' \
                            '--trim[Drop trailing blank lines]'
                    fi
                    ;;
                send|key)
                    (( CURRENT == 2 )) && __looop_sessions
                    ;;
                expect)
                    if (( CURRENT == 2 )); then
                        __looop_sessions
                    else
                        _arguments \
                            '--timeout[Give up after DUR]:duration:' \
                            '--from-now[Only match new output]' \
                            '--raw[Match raw output]' \
                            '--screen[Match the rendered screen]' \
                            '--json[Emit JSON]'
                    fi
                    ;;
                wait)
                    if (( CURRENT == 2 )); then
                        __looop_sessions
                    else
                        _arguments '--timeout[Give up after DUR]:duration:'
                    fi
                    ;;
                wait-idle)
                    if (( CURRENT == 2 )); then
                        __looop_sessions
                    else
                        _arguments \
                            '--settle[Quiet window, e.g. 500ms]:duration:' \
                            '--timeout[Give up after DUR]:duration:'
                    fi
                    ;;
                resize)
                    (( CURRENT == 2 )) && __looop_sessions
                    ;;
                ls)
                    _arguments \
                        '--json[Emit JSON instead of a table]' \
                        '(-w --watch)'{-w,--watch}'[Re-render the table continuously]' \
                        '(-n --interval)'{-n,--interval}'[Refresh interval, e.g. 2s]:duration:'
                    ;;
                status)
                    _arguments '--json[Emit JSON instead of text]'
                    ;;
                cost)
                    _arguments \
                        '1:period:(today all)' \
                        '--json[Emit JSON instead of text]'
                    ;;
                config)
                    if (( CURRENT == 2 )); then
                        local -a shells
                        shells=('zsh:Zsh integration script' 'bash:Bash integration script')
                        _describe 'shell' shells
                    fi
                    ;;
            esac
            ;;
    esac
}

compdef _looop looop
