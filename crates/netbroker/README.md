# agentd-netbroker

`agentd-netbroker` builds the privileged Windows network broker used by the shell sandbox.

The broker runs as a Windows service. It accepts local named-pipe requests from agentd and installs Windows Filtering Platform rules for a sandbox AppContainer. These rules restrict outbound traffic to approved IP addresses.

The broker also installs and removes the access-control entries required by the Windows sandbox. The binary returns an error on other platforms.
