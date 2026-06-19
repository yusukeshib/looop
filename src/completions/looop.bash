# Resolve the session-fleet dir the same way paths.rs does: sessions live at
# <LOOOP_DATA_DIR>/sessions/<id> (the fleet root is the data dir itself).
__looop_sessions_dir() {
    local default="${XDG_STATE_HOME:-$HOME/.local/state}/looop"
    printf '%s' "${LOOOP_DATA_DIR:-$default}/sessions"
}

_looop() {
    local cur prev words cword
    _init_completion || return

    local subcommands="watch ls status journal log shot send key expect wait wait-idle resize restart start-session attach detach kill flag unflag prune cost config version help"

    # session ids including the pulse (for read/observe verbs)
    __looop_session_list() {
        local dir s name out=""
        dir=$(__looop_sessions_dir)
        for s in "$dir"/*/; do
            [[ -d "$s" ]] || continue
            name=$(basename "$s")
            [[ "$name" == "pulse" ]] && continue
            [[ -n "$name" ]] && out+=" $name"
        done
        printf '%s pulse' "$out"
    }

    if [[ $cword -eq 1 ]]; then
        COMPREPLY=($(compgen -W "$subcommands" -- "$cur"))
        return
    fi

    local subcmd="${words[1]}"
    [[ -z "$subcmd" ]] && return

    case "$subcmd" in
        attach|kill|flag|unflag|restart)
            if [[ $cword -eq 2 ]]; then
                local dir workers="" s name
                dir=$(__looop_sessions_dir)
                for s in "$dir"/*/; do
                    [[ -d "$s" ]] || continue
                    name=$(basename "$s")
                    [[ "$name" == "pulse" ]] && continue
                    [[ -n "$name" ]] && workers+=" $name"
                done
                COMPREPLY=($(compgen -W "$workers" -- "$cur"))
            fi
            ;;
        watch|detach|send|key|resize)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_session_list)" -- "$cur"))
            fi
            ;;
        log)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_session_list)" -- "$cur"))
            else
                COMPREPLY=($(compgen -W "--tail --grep --since --follow -f --raw --json" -- "$cur"))
            fi
            ;;
        shot)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_session_list)" -- "$cur"))
            else
                COMPREPLY=($(compgen -W "--ansi --json --trim" -- "$cur"))
            fi
            ;;
        expect)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_session_list)" -- "$cur"))
            else
                COMPREPLY=($(compgen -W "--timeout --from-now --raw --screen --json" -- "$cur"))
            fi
            ;;
        wait)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_session_list)" -- "$cur"))
            else
                COMPREPLY=($(compgen -W "--timeout" -- "$cur"))
            fi
            ;;
        wait-idle)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "$(__looop_session_list)" -- "$cur"))
            else
                COMPREPLY=($(compgen -W "--settle --timeout" -- "$cur"))
            fi
            ;;
        ls)
            COMPREPLY=($(compgen -W "--json --watch -w --interval -n" -- "$cur"))
            ;;
        status)
            COMPREPLY=($(compgen -W "--json" -- "$cur"))
            ;;
        journal)
            COMPREPLY=($(compgen -W "--tail -n" -- "$cur"))
            ;;
        cost)
            COMPREPLY=($(compgen -W "today all --json" -- "$cur"))
            ;;
        config)
            if [[ $cword -eq 2 ]]; then
                COMPREPLY=($(compgen -W "zsh bash" -- "$cur"))
            fi
            ;;
    esac
}
complete -F _looop looop
