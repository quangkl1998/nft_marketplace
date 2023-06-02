use crate::order_state::{
    consideration_item, offer_item, order_key, Asset, ItemType, OrderComponents, OrderType,
    PaymentAsset, CW20, NFT,
};
use crate::{
    state::{listing_key, AuctionConfig, Listing, MarketplaceContract},
    ContractError,
};
use cosmwasm_std::{
    to_binary, Addr, BankMsg, Coin, CosmosMsg, DepsMut, Env, MessageInfo, QueryRequest, Response,
    StdResult, Uint128, WasmMsg, WasmQuery,
};
use cw20::{AllowanceResponse, Cw20ExecuteMsg, Cw20QueryMsg};
use cw2981_royalties::{
    msg::RoyaltiesInfoResponse, ExecuteMsg as Cw2981ExecuteMsg, QueryMsg as Cw2981QueryMsg,
};
use cw721::{Cw721QueryMsg, Expiration as Cw721Expiration};

impl MarketplaceContract<'static> {
    pub fn validate_auction_config(&self, auction_config: &AuctionConfig) -> bool {
        match auction_config {
            AuctionConfig::FixedPrice {
                price,
                start_time,
                end_time,
            } => {
                if price.amount.is_zero() {
                    // since price is Uint128, it cannot be negative, we only
                    // need to check if it's zero
                    return false;
                }
                // if start_time or end_time is not set, we don't need to check
                if start_time.is_some()
                    && end_time.is_some()
                    && start_time.unwrap() >= end_time.unwrap()
                {
                    return false;
                }
                true
            }
        }
    }

    pub fn execute_list_nft(
        self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        contract_address: Addr,
        token_id: String,
        auction_config: AuctionConfig,
    ) -> Result<Response, ContractError> {
        // check if user is the owner of the token
        let query_owner_msg = Cw721QueryMsg::OwnerOf {
            token_id: token_id.clone(),
            include_expired: Some(false),
        };
        let owner_response: StdResult<cw721::OwnerOfResponse> =
            deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: contract_address.to_string(),
                msg: to_binary(&query_owner_msg)?,
            }));
        match owner_response {
            Ok(owner) => {
                if owner.owner != info.sender {
                    return Err(ContractError::Unauthorized {});
                }
            }
            Err(_) => {
                return Err(ContractError::Unauthorized {});
            }
        }

        // check that user approves this contract to manage this token
        // for now, we require never expired approval
        let query_approval_msg = Cw721QueryMsg::Approval {
            token_id: token_id.clone(),
            spender: env.contract.address.to_string(),
            include_expired: Some(true),
        };
        let approval_response: StdResult<cw721::ApprovalResponse> =
            deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: contract_address.to_string(),
                msg: to_binary(&query_approval_msg)?,
            }));

        // check if approval is never expired
        match approval_response {
            Ok(approval) => match approval.approval.expires {
                Cw721Expiration::Never {} => {}
                _ => return Err(ContractError::Unauthorized {}),
            },
            Err(_) => {
                return Err(ContractError::CustomError {
                    val: "Require never expired approval".to_string(),
                });
            }
        }

        if !self.validate_auction_config(&auction_config) {
            return Err(ContractError::CustomError {
                val: "Invalid auction config".to_string(),
            });
        }

        // add a nft to listings
        let listing = Listing {
            contract_address: contract_address.clone(),
            token_id: token_id.clone(),
            auction_config,
            seller: info.sender,
            buyer: None,
        };
        let listing_key = listing_key(&contract_address, &token_id);

        // we will override the listing if it already exists, so that we can update the auction config
        let new_listing = self.listings.update(
            deps.storage,
            listing_key,
            |_old| -> Result<Listing, ContractError> { Ok(listing) },
        )?;

        // println!("Listing: {:?}", _listing);
        let auction_config_str = serde_json::to_string(&new_listing.auction_config);
        match auction_config_str {
            Ok(auction_config_str) => Ok(Response::new()
                .add_attribute("method", "list_nft")
                .add_attribute("contract_address", new_listing.contract_address)
                .add_attribute("token_id", new_listing.token_id)
                .add_attribute("auction_config", auction_config_str)
                .add_attribute("seller", new_listing.seller.to_string())),
            Err(_) => Err(ContractError::CustomError {
                val: ("Auction Config Error".to_string()),
            }),
        }
    }

    pub fn execute_buy(
        self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        contract_address: Addr,
        token_id: String,
    ) -> Result<Response, ContractError> {
        // get the listing
        let listing_key = listing_key(&contract_address, &token_id);
        let mut listing = self.listings.load(deps.storage, listing_key.clone())?;

        // check if buyer is the same as seller
        if info.sender == listing.seller {
            return Err(ContractError::CustomError {
                val: ("Owner cannot buy".to_string()),
            });
        }

        listing.buyer = Some(info.sender.clone());

        // remove the listing
        self.listings.remove(deps.storage, listing_key)?;

        match &listing.auction_config {
            AuctionConfig::FixedPrice { .. } => {
                self.process_buy_fixed_price(deps, env, info, &listing)
            }
        }
    }

    fn process_buy_fixed_price(
        self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        listing: &Listing,
    ) -> Result<Response, ContractError> {
        match &listing.auction_config {
            AuctionConfig::FixedPrice {
                price,
                start_time,
                end_time,
            } => {
                // check if current block is after start_time
                if start_time.is_some() && !start_time.unwrap().is_expired(&env.block) {
                    return Err(ContractError::CustomError {
                        val: ("Auction not started".to_string()),
                    });
                }

                if end_time.is_some() && end_time.unwrap().is_expired(&env.block) {
                    return Err(ContractError::CustomError {
                        val: format!("Auction ended: {} {}", end_time.unwrap(), env.block.time),
                    });
                }

                // check if enough funds
                if info.funds.is_empty() || info.funds[0] != *price {
                    return Err(ContractError::InsufficientFunds {});
                }

                // message to transfer nft to buyer
                let transfer_nft_msg = WasmMsg::Execute {
                    contract_addr: listing.contract_address.to_string(),
                    msg: to_binary(&Cw2981ExecuteMsg::TransferNft {
                        recipient: listing.buyer.clone().unwrap().into_string(),
                        token_id: listing.token_id.clone(),
                    })?,
                    funds: vec![],
                };
                let mut res = Response::new().add_message(transfer_nft_msg);

                let payment = PaymentAsset::Native {
                    denom: price.denom.clone(),
                    amount: price.amount.into(),
                };

                let payment_messages = self.payment_with_royalty(
                    &deps,
                    &listing.contract_address,
                    &listing.token_id,
                    payment,
                    &info.sender,
                    &listing.seller,
                );

                for payment_message in payment_messages {
                    res = res.add_message(payment_message);
                }

                res = res
                    .add_attribute("method", "buy")
                    .add_attribute("contract_address", listing.contract_address.to_string())
                    .add_attribute("token_id", listing.token_id.to_string())
                    .add_attribute("buyer", info.sender);

                Ok(res)
            }
        }
    }

    pub fn execute_cancel(
        self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        contract_address: Addr,
        token_id: String,
    ) -> Result<Response, ContractError> {
        // find listing
        let listing_key = listing_key(&contract_address, &token_id);
        let listing = self.listings.load(deps.storage, listing_key.clone())?;

        // if a listing is not expired, only seller can cancel
        if (!listing.is_expired(&env.block)) && (listing.seller != info.sender) {
            return Err(ContractError::Unauthorized {});
        }

        // we will remove the cancelled listing
        self.listings.remove(deps.storage, listing_key)?;

        Ok(Response::new()
            .add_attribute("method", "cancel")
            .add_attribute("contract_address", contract_address)
            .add_attribute("token_id", token_id)
            .add_attribute("cancelled_at", env.block.time.to_string()))
    }

    // function to add new offer nft using ordering style
    // the 'offer' of offer_nft will contain the information of price
    // the 'consideration' of offer_nft will contain the information of nft
    pub fn execute_offer_nft(
        self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        nft: NFT,
        funds_amount: u128,
        end_time: Cw721Expiration,
    ) -> Result<Response, ContractError> {
        // load config
        let config = self.config.load(deps.storage)?;
        // check ig the vaura_address is set (the default value is equal to "aura0")
        if config.vaura_address == Addr::unchecked("aura0") {
            return Err(ContractError::VauraAddressNotSet {});
        }

        // check if the end time is valid
        if end_time.is_expired(&env.block) {
            return Err(ContractError::InvalidEndTime {});
        }
        // ***********
        // OFFERING FUNDS
        // ***********
        // load config
        let config = self.config.load(deps.storage)?;

        let token_address = config.vaura_address;
        let amount = funds_amount;

        // check that the allowance of the cw20 offer token is enough
        let allowance_response: AllowanceResponse = deps
            .querier
            .query_wasm_smart(
                &token_address,
                &Cw20QueryMsg::Allowance {
                    owner: info.sender.to_string(),
                    spender: env.contract.address.to_string(),
                },
            )
            .unwrap();

        // check if the allowance is greater or equal the offer amount
        if allowance_response.allowance < Uint128::from(amount) {
            return Err(ContractError::InsufficientAllowance {});
        }

        let contract_address = nft.contract_address;
        let token_id = nft.token_id;
        if let Some(token_id) = token_id {
            // query the owner of the nft to check if the nft exist
            let owner_response: StdResult<cw721::OwnerOfResponse> = deps.querier.query_wasm_smart(
                &contract_address,
                &Cw721QueryMsg::OwnerOf {
                    token_id: token_id.clone(),
                    include_expired: Some(false),
                },
            );

            match owner_response {
                Ok(owner) => {
                    if owner.owner == info.sender {
                        return Err(ContractError::CustomError {
                            val: ("Cannot offer owned nft".to_string()),
                        });
                    }
                }
                Err(_) => {
                    return Err(ContractError::CustomError {
                        val: ("Nft not exist".to_string()),
                    });
                }
            }

            // generate order key for order components based on user address, contract address and token id
            let order_key = order_key(&info.sender, &contract_address, &token_id);

            // the offer item will contain the infomation of cw20 token
            let offer_item = offer_item(
                &ItemType::CW20,
                &Asset::Cw20(CW20 {
                    contract_address: token_address,
                    amount,
                }),
                &0u128,
                &0u128,
            );

            // the consideration item will contain the infomation of nft
            let consideration_item = consideration_item(
                &ItemType::CW721,
                &Asset::Nft(NFT {
                    contract_address,
                    token_id: Some(token_id),
                }),
                &0u128,
                &0u128,
                &info.sender,
            );

            // generate order components
            let order_offer = OrderComponents {
                order_type: OrderType::OFFER, // The type of offer must be OFFER
                order_id: order_key.clone(),
                offerer: info.sender,
                offer: [offer_item].to_vec(),
                consideration: [consideration_item].to_vec(),
                start_time: None,
                end_time: Some(end_time),
            };

            // we will override the order if it already exists
            let new_offer = self.offers.update(
                deps.storage,
                order_key,
                |_old| -> Result<OrderComponents, ContractError> { Ok(order_offer) },
            )?;

            let offer_str = serde_json::to_string(&new_offer.offer);
            let consideration_str = serde_json::to_string(&new_offer.consideration);

            // return success
            Ok(Response::new()
                .add_attribute("method", "create_offer")
                .add_attribute("order_type", "OFFER")
                .add_attribute("offerer", new_offer.offerer)
                .add_attribute("offer", offer_str.unwrap())
                .add_attribute("consideration", consideration_str.unwrap())
                .add_attribute("end_time", new_offer.end_time.unwrap().to_string()))
        } else {
            // if the token_id is not exist, then this order is offer for a collection of nft
            // we will handle this in the next version => return error for now
            Err(ContractError::CustomError {
                val: ("Collection offer is not supported".to_string()),
            })
        }
    }

    // function to accept offer nft using ordering style
    pub fn execute_accept_nft_offer(
        self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        offerer: Addr,
        nft: NFT,
        funds_amount: u128,
    ) -> Result<Response, ContractError> {
        let contract_address = nft.contract_address;
        let token_id = nft.token_id;
        // if the token_id is exist, then this order is offer for a specific nft
        if let Some(token_id) = token_id {
            // generate order key for order components based on user address, contract address and token id
            let order_key = order_key(&offerer, &contract_address, &token_id);

            // get order components
            let order_components = self.offers.load(deps.storage, order_key.clone())?;

            // if the end time of the offer is expired, then return error
            if order_components.end_time.unwrap().is_expired(&env.block) {
                return Err(ContractError::CustomError {
                    val: ("Offer is expired".to_string()),
                });
            }
            match &order_components.consideration[0].item {
                // match if the consideration item is Nft
                Asset::Nft(NFT {
                    contract_address,
                    token_id,
                }) => {
                    // query the owner of the nft
                    let owner: cw721::OwnerOfResponse = deps
                        .querier
                        .query_wasm_smart(
                            contract_address,
                            &Cw721QueryMsg::OwnerOf {
                                token_id: token_id.clone().unwrap(),
                                include_expired: Some(false),
                            },
                        )
                        .unwrap();

                    // if the nft is not belong to the info.sender, then return error
                    if owner.owner != info.sender {
                        return Err(ContractError::Unauthorized {});
                    }

                    let mut res: Response = Response::new();

                    // ***********************
                    // TRANSFER CW20 TO SENDER
                    // ***********************
                    // convert Asset to PaymentAsset
                    let payment_item = PaymentAsset::from(order_components.offer[0].item.clone());

                    // execute cw20 transfer msg from offerer to info.sender
                    match &payment_item {
                        PaymentAsset::Cw20 {
                            contract_address: _,
                            amount,
                        } => {
                            if funds_amount != *amount {
                                return Err(ContractError::CustomError {
                                    val: ("Insufficient funds".to_string()),
                                });
                            }
                            let payment_messages = self.payment_with_royalty(
                                &deps,
                                contract_address,
                                token_id.as_ref().unwrap(),
                                payment_item.clone(),
                                &offerer,
                                &info.sender,
                            );

                            // loop through all payment messages and add item to response to execute
                            for payment_message in payment_messages {
                                res = res.add_message(payment_message);
                            }
                        }
                        _ => {
                            return Err(ContractError::CustomError {
                                val: ("Invalid Offer funding type".to_string()),
                            });
                        }
                    }

                    // ***********************
                    // TRANSFER NFT TO OFFERER
                    // ***********************
                    // message to transfer nft to offerer
                    let transfer_nft_msg = WasmMsg::Execute {
                        contract_addr: contract_address.clone().to_string(),
                        msg: to_binary(&Cw2981ExecuteMsg::TransferNft {
                            recipient: order_components.offerer.clone().to_string(),
                            token_id: token_id.clone().unwrap(),
                        })?,
                        funds: vec![],
                    };

                    // add transfer nft message to response to execute
                    res = res.add_message(transfer_nft_msg);

                    // After the offer is accepted, we will delete the order
                    self.offers.remove(deps.storage, order_key)?;

                    let listing_key = listing_key(contract_address, &token_id.clone().unwrap());
                    self.listings.remove(deps.storage, listing_key)?;

                    Ok(res
                        .add_attribute("method", "execute_accept_nft_offer")
                        .add_attribute("owner", owner.owner)
                        .add_attribute("offerer", order_components.offerer)
                        .add_attribute("nft_contract_address", contract_address.to_string())
                        .add_attribute("token_id", token_id.clone().unwrap()))
                }
                // if the consideration item is not Nft, then return error
                _ => Err(ContractError::CustomError {
                    val: ("Consideration is not NFT".to_string()),
                }),
            }
        } else {
            Err(ContractError::CustomError {
                val: ("Collection offer is not supported".to_string()),
            })
        }
    }

    pub fn execute_cancel_offer(
        &self,
        deps: DepsMut,
        env: Env,
        info: MessageInfo,
        nfts: Vec<NFT>,
    ) -> Result<Response, ContractError> {
        // if the number of nfts is greater than 50, then return error
        if nfts.len() > 50 {
            return Err(ContractError::CustomError {
                val: ("Number of NFTs is greater than 50".to_string()),
            });
        }

        // loop through all nfts
        for nft in nfts {
            // generate order key based on the sender address, nft.contract_address and nft.token_id
            let order_key = order_key(&info.sender, &nft.contract_address, &nft.token_id.unwrap());

            // check if the order exists
            if !self.offers.has(deps.storage, order_key.clone()) {
                return Err(ContractError::CustomError {
                    val: ("Offer does not exist".to_string()),
                });
            }

            // we will remove the cancelled offer
            self.offers.remove(deps.storage, order_key)?;
        }

        Ok(Response::new()
            .add_attribute("method", "cancel_all_offer")
            .add_attribute("user", info.sender.to_string())
            .add_attribute("cancelled_at", env.block.time.to_string()))
    }

    pub fn execute_edit_vaura_token(
        &self,
        deps: DepsMut,
        _env: Env,
        info: MessageInfo,
        token_address: String,
    ) -> Result<Response, ContractError> {
        // get owner
        let mut conf = self.config.load(deps.storage)?;

        // check if the sender is the owner
        if conf.owner != info.sender {
            return Err(ContractError::Unauthorized {});
        }

        // update vaura address in config
        conf.vaura_address = Addr::unchecked(&token_address);
        // conf.vaura_address = deps.api.addr_validate(&token_address)?;

        // save config
        self.config.save(deps.storage, &conf)?;

        Ok(Response::new()
            .add_attribute("method", "edit_vaura_token")
            .add_attribute("vaura_token_address", token_address))
    }

    // function to process payment transfer with royalty
    fn payment_with_royalty(
        &self,
        deps: &DepsMut,
        nft_contract_address: &Addr,
        nft_id: &str,
        token: PaymentAsset,
        sender: &Addr,
        receipient: &Addr,
    ) -> Vec<CosmosMsg> {
        // create empty vector of CosmosMsg
        let mut res_messages: Vec<CosmosMsg> = vec![];

        // Extract information from token
        let (is_native, token_info, amount) = match token {
            PaymentAsset::Cw20 {
                contract_address: token_address,
                amount,
            } => (false, token_address.to_string(), Uint128::from(amount)),
            PaymentAsset::Native { denom, amount } => (true, denom, Uint128::from(amount)),
        };

        // get cw2981 royalties info
        let royalty_query_msg = Cw2981QueryMsg::Extension {
            msg: cw2981_royalties::msg::Cw2981QueryMsg::RoyaltyInfo {
                token_id: nft_id.into(),
                sale_price: amount,
            },
        };

        let royalty_info_rsp: Result<RoyaltiesInfoResponse, cosmwasm_std::StdError> =
            deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: nft_contract_address.to_string(),
                msg: to_binary(&royalty_query_msg).unwrap(),
            }));

        let (creator, royalty_amount): (Option<Addr>, Option<Uint128>) = match royalty_info_rsp {
            Ok(RoyaltiesInfoResponse {
                address,
                royalty_amount,
            }) => {
                if address.is_empty() || royalty_amount == Uint128::zero() {
                    (None, None)
                } else {
                    (
                        Some(deps.api.addr_validate(&address).unwrap()),
                        Some(royalty_amount),
                    )
                }
            }
            Err(_) => (None, None),
        };

        // there is no royalty, creator is the receipient, or royalty amount is 0
        if creator.is_none()
            || *creator.as_ref().unwrap() == *receipient
            || royalty_amount.is_none()
            || royalty_amount.unwrap().is_zero()
        {
            match &is_native {
                false => {
                    // execute cw20 transfer msg from info.sender to receipient
                    let transfer_response = WasmMsg::Execute {
                        contract_addr: deps.api.addr_validate(&token_info).unwrap().to_string(),
                        msg: to_binary(&Cw20ExecuteMsg::TransferFrom {
                            owner: sender.to_string(),
                            recipient: receipient.to_string(),
                            amount,
                        })
                        .unwrap(),
                        funds: vec![],
                    };
                    res_messages.push(transfer_response.into());
                }
                true => {
                    // transfer all funds to receipient
                    let transfer_response = BankMsg::Send {
                        to_address: receipient.to_string(),
                        amount: vec![Coin {
                            denom: token_info,
                            amount,
                        }],
                    };
                    res_messages.push(transfer_response.into());
                }
            }
        } else if let (Some(creator), Some(royalty_amount)) = (creator, royalty_amount) {
            match &is_native {
                false => {
                    // execute cw20 transfer transfer royalty to creator
                    let transfer_token_creator_response = WasmMsg::Execute {
                        contract_addr: deps.api.addr_validate(&token_info).unwrap().to_string(),
                        msg: to_binary(&Cw20ExecuteMsg::TransferFrom {
                            owner: sender.to_string(),
                            recipient: creator.to_string(),
                            amount: royalty_amount,
                        })
                        .unwrap(),
                        funds: vec![],
                    };
                    res_messages.push(transfer_token_creator_response.into());

                    // execute cw20 transfer remaining funds to receipient
                    let transfer_token_seller_msg = WasmMsg::Execute {
                        contract_addr: deps.api.addr_validate(&token_info).unwrap().to_string(),
                        msg: to_binary(&Cw20ExecuteMsg::TransferFrom {
                            owner: sender.to_string(),
                            recipient: receipient.to_string(),
                            amount: amount - royalty_amount,
                        })
                        .unwrap(),
                        funds: vec![],
                    };
                    res_messages.push(transfer_token_seller_msg.into());
                }
                true => {
                    // transfer royalty to creator
                    let transfer_token_creator_response = BankMsg::Send {
                        to_address: creator.to_string(),
                        amount: vec![Coin {
                            denom: token_info.clone(),
                            amount: royalty_amount,
                        }],
                    };
                    res_messages.push(transfer_token_creator_response.into());

                    // transfer remaining funds to receipient
                    let transfer_token_seller_msg = BankMsg::Send {
                        to_address: receipient.to_string(),
                        amount: vec![Coin {
                            denom: token_info,
                            amount: amount - royalty_amount,
                        }],
                    };
                    res_messages.push(transfer_token_seller_msg.into());
                }
            }
        }

        res_messages
    }
}
