# looop zsh completion + shell integration.
# Enable with:  eval "$(looop config zsh)"

__looop_data_dir() {
    if [[ -n "$LOOOP_DATA_DIR" ]]; then
        print -r -- "$LOOOP_DATA_DIR"
    else
        print -r -- "${XDG_STATE_HOME:-$HOME/.local/state}/looop"
    fi
}

# Pending asks: an ask (asks/<id>.json) with no matching answer (answers/<id>.json).
__looop_asks() {
    local -a asks
    local root; root=$(__looop_data_dir)
    if [[ -d "$root/asks" ]]; then
        local f name
        for f in "$root/asks"/*.json(N.); do
            name=${f:t:r}
            [[ -f "$root/answers/$name.json" ]] && continue
            asks+=("$name")
        done
    fi
    (( ${#asks} )) && _describe 'pending ask' asks
}

__looop_goals() {
    local -a goals
    local root; root=$(__looop_data_dir)
    if [[ -d "$root/goals" ]]; then
        local f
        for f in "$root/goals"/*.md(N.); do
            goals+=("${f:t:r}")
        done
    fi
    (( ${#goals} )) && _describe 'goal' goals
}

__looop_sensors() {
    local -a sensors
    local root; root=$(__looop_data_dir)
    if [[ -d "$root/sensors" ]]; then
        local f
        for f in "$root/sensors"/*.sh(N.); do
            sensors+=("${f:t:r}")
        done
    fi
    (( ${#sensors} )) && _describe 'sensor' sensors
}

# Live worker / session ids (sessions/<id>/).
__looop_workers() {
    local -a workers
    local root; root=$(__looop_data_dir)
    if [[ -d "$root/sessions" ]]; then
        local d
        for d in "$root/sessions"/*(N/); do
            workers+=("${d:t}")
        done
    fi
    (( ${#workers} )) && _describe 'worker' workers
}

__looop_claims() {
    local -a claims
    local root; root=$(__looop_data_dir)
    if [[ -d "$root/claims" ]]; then
        local f
        for f in "$root/claims"/*(N.); do
            claims+=("${f:t}")
        done
    fi
    (( ${#claims} )) && _describe 'lease' claims
}

_looop() {
    local curcontext="$curcontext" state line
    typeset -A opt_args

    _arguments -C \
        '1: :->subcmd' \
        '*:: :->args'

    case $state in
        subcmd)
            local -a subcmds
            subcmds=(
                'init:Interactive setup: choose the agent runner'
                'up:Bring the autonomous pulse up'
                'down:Tear the pulse (and workers) down'
                'state:Full world snapshot: goals, sensors, fleet, asks'
                'wait:Block until the world changes, then print state'
                'asks:Just the pending asks'
                'answer:Answer a pending ask'
                'goal:Create/replace or archive a goal'
                'sensor:Create/replace a sensor script'
                'playbook:Rewrite the PLAYBOOK'
                'run:One ad-hoc, reversible shell command'
                'worker:Spawn / kill / list workers'
                'screenshot:Capture a worker'"'"'s current screen'
                'kill:Kill a session by id'
                'claim:Atomically claim a named lease'
                'unclaim:Release a named lease'
                'config:Output shell configuration'
                'version:Print the version'
                'help:Show the manual'
            )
            _describe 'subcommand' subcmds
            ;;
        args)
            case $words[1] in
                up)
                    _arguments '--json[Emit pulse logs as JSON]'
                    ;;
                state)
                    _arguments '--json[Emit JSON instead of the human summary]'
                    ;;
                wait)
                    _arguments \
                        '--json[Emit JSON]' \
                        '--actionable[Wake on asks/journal moves]' \
                        '--only-asks[Wake only on a new pending ask]'
                    ;;
                asks)
                    _arguments '--json[Emit JSON]'
                    ;;
                answer)
                    if (( CURRENT == 2 )); then
                        __looop_asks
                    else
                        _arguments '--force[Overwrite an already-given answer]'
                    fi
                    ;;
                goal)
                    if (( CURRENT == 2 )); then
                        _describe 'goal op' '(write:Create or replace a goal archive:Archive a goal)'
                    elif (( CURRENT == 3 )) && [[ $words[2] == archive ]]; then
                        __looop_goals
                    elif (( CURRENT == 3 )) && [[ $words[2] == write ]]; then
                        __looop_goals
                    fi
                    ;;
                sensor)
                    if (( CURRENT == 2 )); then
                        _describe 'sensor op' '(write:Create or replace a sensor)'
                    elif (( CURRENT == 3 )) && [[ $words[2] == write ]]; then
                        __looop_sensors
                    fi
                    ;;
                playbook)
                    if (( CURRENT == 2 )); then
                        _describe 'playbook op' '(write:Rewrite the PLAYBOOK)'
                    fi
                    ;;
                worker)
                    if (( CURRENT == 2 )); then
                        _describe 'worker op' '(start:Spawn a worker kill:Kill a worker list:List the fleet)'
                    elif [[ $words[2] == kill ]] && (( CURRENT == 3 )); then
                        __looop_workers
                    elif [[ $words[2] == list ]]; then
                        _arguments \
                            '--json[Emit JSON]' \
                            '(-a --all)'{-a,--all}'[Show finished/dead workers too]' \
                            '--watch[Re-render until Ctrl-C]' \
                            '--interval[Refresh interval seconds]:seconds:'
                    elif [[ $words[2] == start ]]; then
                        _arguments \
                            '--model[Model to launch this worker with]:model:' \
                            '--thinking[Thinking level]:level:'
                    fi
                    ;;
                screenshot)
                    if (( CURRENT == 2 )); then
                        __looop_workers
                    else
                        _arguments \
                            '--ansi[Emit ANSI-colored output]' \
                            '--json[Emit JSON]' \
                            '--plain[Emit plain text (default)]' \
                            '--no-trim[Keep trailing blank lines]'
                    fi
                    ;;
                kill)
                    (( CURRENT == 2 )) && __looop_workers
                    ;;
                claim|unclaim)
                    (( CURRENT == 2 )) && __looop_claims
                    ;;
                config)
                    if (( CURRENT == 2 )); then
                        _describe 'shell' '(zsh:Zsh completion script bash:Bash completion script)'
                    fi
                    ;;
            esac
            ;;
    esac
}
compdef _looop looop
