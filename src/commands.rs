/* GUIDE: How to make your own command parser function
 * 
 * The handle function below is the all-encompassing handler for all commands.
 * Commands are parsed as follows:
 * "/command arg1 arg2 ..." -> vec!["command", "arg1", "arg2", ...]
 * 
 * Commands are identified by the first argument (name)
 */

use serde_json::json;

use crate::*;

const USER_COMMANDS: [&str; 4] = [
    "User commands:",
    "/test",
    "/help",
    "/dm <user>"
];

const ADMIN_COMMANDS: [&str; 7] = [
    "Admin commands:",
    "/newchannel <channel_name>",
    "/kick <user>",
    "/ban <user>",
    "/unban <user>",
    "/banip <user>",
    "/unbanip <user>"
];

fn new_msg(msg: &str, app: &mut App) {
    app.add_msg_to_current_channel(Msg::command_msg(msg));
}   

pub async fn handle(args: Vec<&str>, app: &mut App) {
    let require = |arg_count: usize| {        
        return args.len() < arg_count;
    };

    let mut user_action = async |action: &str| {
        if require(2) {return}
        app.send_json(json!({
            "action": action,
            "user": &args[1],
            "session": app.session
        })).await;
    };

    match args[0] {
        "kick" => {
            user_action("kick").await;
        }
        "ban" => {
            user_action("ban").await;
        }
        "banip" => {
            user_action("banip").await;
        }
        "unban" => {
            user_action("unban").await;
        }
        "unbanip" => {
            user_action("unbanip").await;
        }
        "dm" => {
            user_action("dm").await;
        }
        "flushkey" => {
            // placeholder for:
            // when you think the key for the current channel is wrong, 
            // flush it and get a new one from someone else.
        },
        "newchannel" => {
            if require(2) {return};
            app.send_json(json!({
                "action": "newchannel",
                "channelname": &args[1],
                "session": app.session
            })).await;
        },
        "test" => new_msg("test", app),
        "help" => {
            new_msg(&USER_COMMANDS.join(" "), app);
            new_msg(&ADMIN_COMMANDS.join(" "), app);
        },
        _ => new_msg(&format!("Unknown command: {}", args[0]), app)
    }
}