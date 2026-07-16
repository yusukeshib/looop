# looop bash completion + shell integration.
# Enable with:  eval "$(looop config bash)"

__looop_data_dir() {
    if [[ -n "$LOOOP_DATA_DIR" ]]; then
        echo "$LOOOP_DATA_DIR"
    else
        echo "${XDG_STATE_HOME:-$HOME/.local/state}/looop"
    fi
}

# Pending asks: an ask (asks/<id>.json) with no matching answer (answers/<id>.json).
__looop_asks_list() {
    local root; root=$(__looop_data_dir)
    local out="" f name
    if [[ -d "$root/asks" ]]; then
        for f in "$root/asks"/*.json; do
            [[ -f "$f" ]] || continue
            name=$(basename "$f" .json)
            [[ -f "$root/answers/$name.json" ]] && continue
            out+=" $name"
        done
    fi
    echo "$out"
}

__looop_goals_list() {
    local root; root=$(__looop_data_dir)
    local out="" f
    if [[ -d "$root/goals" ]]; then
        for f in "$root/goals"/*.md; do
            [[ -f "$f" ]] && out+=" $(basename "$f" .md)"
        done
    fi
    echo "$out"
}

__looop_sensors_list() {
    local root; root=$(__looop_data_dir)
    local out="" f
    if [[ -d "$root/sensors" ]]; then
        for f in "$root/sensors"/*.sh; do
            [[ -f "$f" ]] && out+=" $(basename "$f" .sh)"
        done
    fi
    echo "$out"
}

__looop_workers_list() {
    local root; root=$(__looop_data_dir)
    local out="" d
    if [[ -d "$root/sessions" ]]; then
        for d in "$root/sessions"/*/; do
            [[ -d "$d" ]] && out+=" $(basename "$d")"
        done
    fi
    echo "$out"
}

__looop_claims_list() {
    local root; root=$(__looop_data_dir)
    local out="" f
    if [[ -d "$root/claims" ]]; then
        for f in "$root/claims"/*; do
            [[ -f "$f" ]] && out+=" $(basename "$f")"
        done
    fi
    echo "$out"
}

_looop() {
    local cur prev words cword
    _init_completion || return

    local subcommands="init up down state wait asks answer goal sensor playbook run worker w screenshot ss kill claim unclaim config version help"

    if [[ $cword -eq 1 ]]; then
        COMPREPLY=($(compgen -W "$subcommands" -- "$cur"))
        return
    fi

    local sub="${words[1]}"
    case "$sub" in
        up)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--json" -- "$cur"))
            ;;
        state)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--json" -- "$cur"))
            ;;
        wait)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--json --actionable --only-asks" -- "$cur"))
            ;;
        asks)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--json" -- "$cur"))
            ;;
        answer)
            if [[ "$cur" == -* ]]; then
                COMPREPLY=($(compgen -W "--force" -- "$cur"))
            elif [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_asks_list)" -- "$cur"))
            fi
            ;;
        goal)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "write w archive" -- "$cur"))
            elif [[ $cword -eq 3 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_goals_list)" -- "$cur"))
            fi
            ;;
        sensor)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "write w" -- "$cur"))
            elif [[ $cword -eq 3 && ( "${words[2]}" == write || "${words[2]}" == w ) ]]; then
                COMPREPLY=($(compgen -W "$(__looop_sensors_list)" -- "$cur"))
            fi
            ;;
        playbook)
            [[ $cword -eq 2 ]] && COMPREPLY=($(compgen -W "write w" -- "$cur"))
            ;;
        worker|w)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "start kill list ls" -- "$cur"))
                return
            fi
            case "${words[2]}" in
                kill)
                    [[ $cword -eq 3 ]] && COMPREPLY=($(compgen -W "$(__looop_workers_list)" -- "$cur"))
                    ;;
                list|ls)
                    [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--json --all -a --watch --interval" -- "$cur"))
                    ;;
                start)
                    [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--model --thinking" -- "$cur"))
                    ;;
            esac
            ;;
        screenshot|ss)
            if [[ "$cur" == -* ]]; then
                COMPREPLY=($(compgen -W "--ansi --json --plain --no-trim" -- "$cur"))
            elif [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_workers_list)" -- "$cur"))
            fi
            ;;
        kill)
            [[ $cword -eq 2 ]] && COMPREPLY=($(compgen -W "$(__looop_workers_list)" -- "$cur"))
            ;;
        claim|unclaim)
            [[ $cword -eq 2 ]] && COMPREPLY=($(compgen -W "$(__looop_claims_list)" -- "$cur"))
            ;;
        config)
            [[ $cword -eq 2 ]] && COMPREPLY=($(compgen -W "zsh bash" -- "$cur"))
            ;;
    esac
}
complete -F _looop looop
