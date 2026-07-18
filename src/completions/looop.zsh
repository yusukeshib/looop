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

__looop_schedules() {
    local -a schedules
    local root; root=$(__looop_data_dir)
    if [[ -d "$root/schedules" ]]; then
        local f
        for f in "$root/schedules"/*.json(N.); do
            schedules+=("${f:t:r}")
        done
    fi
    (( ${#schedules} )) && _describe 'schedule' schedules
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
                'schedule:Durable time triggers (one-shot / recurring)'
                'run:One ad-hoc, reversible shell command'
                'worker:Spawn / kill / list workers'
                'w:Spawn / kill / list workers (alias of worker)'
                'ask:Raise a blocking ask for the human (worker self-callback)'
                'tell:Queue a steering message into a live worker'
                'told:Print + consume pending steering messages'
                'screenshot:Capture a worker'"'"'s current screen'
                'ss:Capture a worker'"'"'s screen (alias of screenshot)'
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
            # The `*::` spec above re-scopes $words/$CURRENT to the subcommand:
            # here $words[1] is the subcommand (not `looop`) and $words[2] is its
            # first argument, so CURRENT==2 is that first argument.
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
                ask)
                    if (( CURRENT == 2 )); then
                        __looop_workers
                    else
                        _arguments \
                            '--prompt[What you need to know from the human]:prompt:' \
                            '--ref[A path/reference the human should look at]:reference:' \
                            '--options[Comma-separated choices to offer]:options:' \
                            '--detach[Write the ask and return immediately]'
                    fi
                    ;;
                tell)
                    (( CURRENT == 2 )) && __looop_workers
                    ;;
                told)
                    (( CURRENT == 2 )) && __looop_workers
                    ;;
                run)
                    _arguments \
                        '--reason[Why this command is being run (recorded)]:reason:' \
                        '--journal[One line: what you did and why]:note:'
                    ;;
                schedule)
                    if (( CURRENT == 2 )); then
                        local -a schedule_ops
                        schedule_ops=(
                            'write:Create or replace a schedule'
                            'w:Create or replace a schedule (alias of write)'
                            'rm:Remove a schedule'
                            'list:List schedules'
                            'ls:List schedules (alias of list)'
                        )
                        _describe 'schedule op' schedule_ops
                    elif (( CURRENT == 3 )) && [[ $words[2] == (write|w|rm) ]]; then
                        __looop_schedules
                    elif [[ $words[2] == (list|ls) ]]; then
                        _arguments '--json[Emit JSON]'
                    elif [[ $words[2] == (write|w) ]]; then
                        _arguments \
                            '--in[One-shot: fire once, this many seconds from now]:seconds:' \
                            '--every[Recurring: fire every N seconds (min 60)]:seconds:' \
                            '--note[Why this trigger exists]:note:' \
                            '--journal[One line: what you did and why]:note:'
                    elif [[ $words[2] == rm ]]; then
                        _arguments '--journal[One line: what you did and why]:note:'
                    fi
                    ;;
                goal)
                    if (( CURRENT == 2 )); then
                        local -a goal_ops
                        goal_ops=(
                            'write:Create or replace a goal'
                            'w:Create or replace a goal (alias of write)'
                            'archive:Archive a goal'
                        )
                        _describe 'goal op' goal_ops
                    elif (( CURRENT == 3 )) && [[ $words[2] == (archive|write|w) ]]; then
                        __looop_goals
                    elif [[ $words[2] == (archive|write|w) ]]; then
                        _arguments '--journal[One line: what you did and why]:note:'
                    fi
                    ;;
                sensor)
                    if (( CURRENT == 2 )); then
                        local -a sensor_ops
                        sensor_ops=(
                            'write:Create or replace a sensor'
                            'w:Create or replace a sensor (alias of write)'
                        )
                        _describe 'sensor op' sensor_ops
                    elif (( CURRENT == 3 )) && [[ $words[2] == (write|w) ]]; then
                        __looop_sensors
                    elif [[ $words[2] == (write|w) ]]; then
                        _arguments '--journal[One line: what you did and why]:note:'
                    fi
                    ;;
                playbook)
                    if (( CURRENT == 2 )); then
                        local -a playbook_ops
                        playbook_ops=(
                            'write:Rewrite the PLAYBOOK'
                            'w:Rewrite the PLAYBOOK (alias of write)'
                        )
                        _describe 'playbook op' playbook_ops
                    elif [[ $words[2] == (write|w) ]]; then
                        _arguments '--journal[One line: what you did and why]:note:'
                    fi
                    ;;
                worker|w)
                    if (( CURRENT == 2 )); then
                        local -a worker_ops
                        worker_ops=(
                            'start:Spawn a worker'
                            'kill:Kill a worker'
                            'list:List the fleet'
                            'ls:List the fleet (alias of list)'
                        )
                        _describe 'worker op' worker_ops
                    elif [[ $words[2] == kill ]] && (( CURRENT == 3 )); then
                        __looop_workers
                    elif [[ $words[2] == (list|ls) ]]; then
                        _arguments \
                            '--json[Emit JSON]' \
                            '(-a --all)'{-a,--all}'[Show finished/dead workers too]' \
                            '(-w --watch)'{-w,--watch}'[Re-render until Ctrl-C]' \
                            '--interval[Refresh interval seconds]:seconds:'
                    elif [[ $words[2] == start ]]; then
                        _arguments \
                            '--command[Full launch-command override (must contain {{prompt_file}})]:command:' \
                            '--verify[Post-condition shell command]:command:' \
                            '--resume[Resume a detached, answered ask (ask id)]:ask:' \
                            '--journal[One line: what you did and why]:note:'
                    fi
                    ;;
                screenshot|ss)
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
                        local -a shells
                        shells=(
                            'zsh:Zsh completion script'
                            'bash:Bash completion script'
                        )
                        _describe 'shell' shells
                    fi
                    ;;
            esac
            ;;
    esac
}
compdef _looop looop
