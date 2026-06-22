#compdef looop

# looop's human surface is tiny: start/stop the pulse, check spend, shell
# integration. Everything else is driven by the root agent you run separately
# (the `looop _ …` verbs), not completed here.
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
                'up:Start the pulse (sensing loop, detached)'
                'down:Stop the pulse and all workers'
                'cost:Report LLM spend from the cost ledger'
                'config:Output shell integration (eval "$(looop config zsh)")'
                'version:Print the looop version'
                'help:Show the full design manual + root-agent contract'
            )
            _describe 'command' cmds
            ;;
        args)
            case $words[1] in
                up)
                    _arguments '--json[Pulse logs NDJSON]'
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
