# Resolve looop's data dir the same way paths.rs does:
#   $LOOOP_DATA_DIR  or  ${XDG_STATE_HOME:-$HOME/.local/state}/looop
__looop_data_dir() {
    local default="${XDG_STATE_HOME:-$HOME/.local/state}/looop"
    printf '%s' "${LOOOP_DATA_DIR:-$default}"
}

# Resolve the babysit fleet root the same way paths.rs does: a non-default
# profile (distinct LOOOP_DATA_DIR) gets its own <data>/babysit root; the
# default profile honors $BABYSIT_DIR, else ~/.babysit.
__looop_babysit_root() {
    local default="${XDG_STATE_HOME:-$HOME/.local/state}/looop"
    local data="${LOOOP_DATA_DIR:-$default}"
    if [[ "$data" != "$default" ]]; then
        printf '%s' "$data/babysit"
    else
        printf '%s' "${BABYSIT_DIR:-$HOME/.babysit}"
    fi
}

_looop() {
    local cur prev words cword
    _init_completion || return

    local subcommands="up down watch run tick ls status log shot send key expect wait wait-idle resize restart start-session attach detach kill flag unflag prune cost config version help"

    # session ids including the pulse (for read/observe verbs)
    __looop_session_list() {
        local root s name out=""
        root=$(__looop_babysit_root)
        for s in "$root"/sessions/looop-*/; do
            [[ -d "$s" ]] || continue
            name=$(basename "$s")
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
        run)
            if [[ $cword -eq 2 ]]; then
                local data goals="" g name
                data=$(__looop_data_dir)
                for g in "$data"/goals/*.md "$data"/goals/archive/*.md; do
                    [[ -f "$g" ]] || continue
                    name=$(basename "$g" .md)
                    [[ -n "$name" ]] && goals+=" $name"
                done
                COMPREPLY=($(compgen -W "$goals" -- "$cur"))
            fi
            ;;
        attach|kill|flag|unflag|restart)
            if [[ $cword -eq 2 ]]; then
                local root workers="" s name
                root=$(__looop_babysit_root)
                for s in "$root"/sessions/looop-*/; do
                    [[ -d "$s" ]] || continue
                    name=$(basename "$s")
                    [[ "$name" == "looop-pulse" ]] && continue
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
        up)
            COMPREPLY=($(compgen -W "--watch -w --json" -- "$cur"))
            ;;
        ls)
            COMPREPLY=($(compgen -W "--json --watch -w --interval -n" -- "$cur"))
            ;;
        status)
            COMPREPLY=($(compgen -W "--json" -- "$cur"))
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
