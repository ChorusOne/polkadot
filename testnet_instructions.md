## Start local polkadot testnet with four validators

### Code changes done
1. Initial validators are now 4 Alice, Bob, Charlie and Dave instead of 2.
2. validator_count changed to 4 from 2 and minimum_validators changed to 2 from 1.
3. Block time changed from 6000 miliseconds to 3000 miliseconds
4. Session duration changed from 4 hours to 1 minute.
5. Number of session in era is reduced from 6 to 2, effectively making era duration to 2 minutes.

### How to run validators?
1. Start Alice validator
`polkadot --alice --chain polkadot-local --base-path <Storage Dir>/alice`
2. Start Bob validator
`polkadot --bob --chain polkadot-local  --port 30334 --rpc-port 9934 --ws-port 9945 --base-path <Storage Dir>/bob`
3. Start Charlie validator
`polkadot --charlie --chain polkadot-local  --port 30335 --rpc-port 9935 --ws-port 9946 --base-path <Storage Dir>/charlie`
4. Start Dave validator
`polkadot --dave --chain polkadot-local  --port 30336 --rpc-port 9936 --ws-port 9947 --base-path <Storage Dir>/dave`

#### Note
By default node's p2p port is at `30333`, rpc port is `9933` and websocket port is `9944`, so we do not pass explicit CLI argument for the `alice` validator
We are running all validators in one machine only. If you want to run it in different machine add following arguments: 
`--rpc-methods Unsafe --unsafe-ws-external --unsafe-rpc-external` which makes it listen rpc and websocket on all network interface.
