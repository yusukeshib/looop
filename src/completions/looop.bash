# looop's human surface is tiny: start/stop the autonomous pulse, watch it, check
# spend, shell integration. The `looop _ …` steer/worker verbs (used by you, a
# concierge, or workers) are not completed here.
_looop() {
    local cur prev words cword
    _init_completion || return

    local subcommands="up down watch cost config version help"

    if [[ $cword -eq 1 ]]; then
        COMPREPLY=($(compgen -W "$subcommands" -- "$cur"))
        return
    fi

    case "${words[1]}" in
        up)
            COMPREPLY=($(compgen -W "--json" -- "$cur"))
            ;;
        watch)
            COMPREPLY=($(compgen -W "--since --all" -- "$cur"))
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
