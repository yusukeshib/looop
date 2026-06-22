# looop's human surface is tiny: start/stop the pulse, check spend, shell
# integration. Everything else is driven by the root agent you run separately
# (the `looop _ …` verbs), not completed here.
_looop() {
    local cur prev words cword
    _init_completion || return

    local subcommands="up down cost config version help"

    if [[ $cword -eq 1 ]]; then
        COMPREPLY=($(compgen -W "$subcommands" -- "$cur"))
        return
    fi

    case "${words[1]}" in
        up)
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
